//! Camera settings through UVC class requests: `GET /?wb=auto&zoom=200`
//! applies them, `GET /controls` reports what the camera offers.
//!
//! Requests go to the VideoControl interface's Camera Terminal (exposure,
//! focus, zoom, pan, tilt...) or Processing Unit (brightness, white
//! balance...); their IDs come from the configuration descriptor that
//! `camera.rs` reads when a camera is enumerated.

use crate::camera::Camera;
use adapter_core::uvc::{self, AutoKind, ControlDef, Entity, Setting, Unit, VcInfo};
use esp_idf_svc::sys::{self, usb};
use log::{info, warn};

const REQ_SET_CUR: u8 = 0x01;
const REQ_GET_CUR: u8 = 0x81;
const REQ_GET_MIN: u8 = 0x82;
const REQ_GET_MAX: u8 = 0x83;
const REQ_GET_RES: u8 = 0x84;
const REQ_GET_DEF: u8 = 0x87;
/// Class request to an interface, host to device / device to host.
const RT_OUT: u8 = 0x21;
const RT_IN: u8 = 0xa1;

/// HTTP status and message for a failed request.
pub type Failure = (u16, String);

impl Camera {
    fn entity(&self, vc: &VcInfo, unit: Unit) -> Option<Entity> {
        match unit {
            Unit::CameraTerminal => vc.camera_terminal,
            Unit::ProcessingUnit => vc.processing_unit,
        }
    }

    fn supported(&self, vc: &VcInfo, unit: Unit, bit: u8) -> Option<Entity> {
        self.entity(vc, unit)
            .filter(|e| e.controls & (1u32 << bit) != 0)
    }

    /// One class request. `data` is sent for SET requests and filled for GETs.
    fn request(
        &self,
        vc: &VcInfo,
        entity: Entity,
        request: u8,
        selector: u8,
        data: &mut [u8],
    ) -> Result<(), String> {
        let request_type = if request == REQ_SET_CUR { RT_OUT } else { RT_IN };
        let w_value = (selector as u16) << 8;
        let w_index = ((entity.id as u16) << 8) | vc.interface as u16;
        self.with_handle(|h| {
            let rc = unsafe {
                usb::uvc_host_usb_ctrl(
                    h,
                    request_type,
                    request,
                    w_value,
                    w_index,
                    data.len() as u16,
                    data.as_mut_ptr(),
                )
            };
            if rc == sys::ESP_OK {
                Ok(())
            } else {
                Err(anyhow::anyhow!(
                    "request {request:#04x} selector {selector:#04x} unit {}: esp_err {rc}",
                    entity.id
                ))
            }
        })
        .map_err(|e| e.to_string())
    }

    fn get(&self, vc: &VcInfo, entity: Entity, request: u8, def: &ControlDef) -> Result<i64, String> {
        let mut data = vec![0u8; def.total as usize];
        self.request(vc, entity, request, def.selector, &mut data)?;
        uvc::decode(def, &data).ok_or_else(|| "short reply".into())
    }

    fn vc_or_fail(&self) -> Result<VcInfo, Failure> {
        if !self.is_attached() {
            return Err((503, "no camera attached".into()));
        }
        self.vc_info()
            .ok_or((503, "camera has no VideoControl interface".into()))
    }

    /// Applies query parameters in order. Returns one line per applied
    /// setting, or the first failure (400 for bad or unsupported parameters,
    /// 500 when the camera rejects the request).
    pub fn apply_settings(&self, params: &[(String, String)]) -> Result<Vec<String>, Failure> {
        let settings: Vec<(&'static ControlDef, Setting)> = params
            .iter()
            .filter(|(k, _)| !uvc::OTHER_PARAMS.contains(&k.as_str()))
            .map(|(k, v)| uvc::parse_setting(k, v).map_err(|e| (400, e)))
            .collect::<Result<_, _>>()?;
        if settings.is_empty() {
            return Ok(Vec::new());
        }
        let vc = self.vc_or_fail()?;
        let mut applied = Vec::new();
        for (def, setting) in &settings {
            let line = self.apply_one(&vc, def, *setting)?;
            info!("camera: {line}");
            applied.push(line);
        }
        self.remember_settings(params);
        Ok(applied)
    }

    fn apply_one(&self, vc: &VcInfo, def: &ControlDef, setting: Setting) -> Result<String, Failure> {
        let entity = self
            .supported(vc, def.unit, def.bit)
            .ok_or_else(|| (400, format!("{} is not supported by this camera", def.name)))?;
        let camera_err = |e: String| (500, format!("camera rejected {}: {e}", def.name));
        match setting {
            Setting::Auto => {
                let auto = def.auto.expect("checked by parse_setting");
                self.supported(vc, def.unit, auto.bit).ok_or_else(|| {
                    (400, format!("{} has no automatic mode on this camera", def.name))
                })?;
                let mut byte = match auto.kind {
                    AutoKind::Flag => [1u8],
                    AutoKind::AeMode => {
                        let mut modes = [0u8];
                        self.request(vc, entity, REQ_GET_RES, auto.selector, &mut modes)
                            .map_err(camera_err)?;
                        [uvc::auto_ae_mode(modes[0]).ok_or_else(|| {
                            (400, "this camera has no automatic exposure mode".to_string())
                        })?]
                    }
                };
                self.request(vc, entity, REQ_SET_CUR, auto.selector, &mut byte)
                    .map_err(camera_err)?;
                Ok(format!("{}=auto", def.name))
            }
            Setting::Value(v) => {
                // Leave the automatic mode first, where the camera has one.
                if let Some(auto) = def.auto {
                    if self.supported(vc, def.unit, auto.bit).is_some() {
                        let mut byte = match auto.kind {
                            AutoKind::Flag => [0u8],
                            AutoKind::AeMode => [1u8],
                        };
                        if let Err(e) = self.request(vc, entity, REQ_SET_CUR, auto.selector, &mut byte)
                        {
                            warn!("camera: could not switch {} to manual: {e}", def.name);
                        }
                    }
                }
                let min = self.get(vc, entity, REQ_GET_MIN, def).map_err(camera_err)?;
                let max = self.get(vc, entity, REQ_GET_MAX, def).map_err(camera_err)?;
                if v < min || v > max {
                    return Err((
                        400,
                        format!(
                            "{}={} is out of range; this camera accepts {}..{}",
                            def.name,
                            uvc::display_value(def, v),
                            uvc::display_value(def, min),
                            uvc::display_value(def, max)
                        ),
                    ));
                }
                let mut data = vec![0u8; def.total as usize];
                if def.total != def.len {
                    // Pan and tilt travel together: keep the other one.
                    self.request(vc, entity, REQ_GET_CUR, def.selector, &mut data)
                        .map_err(camera_err)?;
                }
                uvc::encode(def, v, &mut data);
                self.request(vc, entity, REQ_SET_CUR, def.selector, &mut data)
                    .map_err(camera_err)?;
                Ok(format!("{}={}", def.name, uvc::display_value(def, v)))
            }
        }
    }

    /// The supported controls with current value and range, as JSON.
    pub fn controls_json(&self) -> Result<String, Failure> {
        let vc = self.vc_or_fail()?;
        let mut out = String::from("{\n");
        let mut first = true;
        for def in uvc::CONTROLS {
            let Some(entity) = self.supported(&vc, def.unit, def.bit) else {
                continue;
            };
            let field = |req: u8| self.get(&vc, entity, req, def).ok();
            let mut props = Vec::new();
            if let Some(auto) = def.auto {
                if self.supported(&vc, def.unit, auto.bit).is_some() {
                    let mut byte = [0u8];
                    if self
                        .request(&vc, entity, REQ_GET_CUR, auto.selector, &mut byte)
                        .is_ok()
                    {
                        let is_auto = match auto.kind {
                            AutoKind::Flag => byte[0] != 0,
                            AutoKind::AeMode => byte[0] != 1,
                        };
                        props.push(format!("\"auto\": {is_auto}"));
                    }
                }
            }
            for (name, req) in [
                ("cur", REQ_GET_CUR),
                ("min", REQ_GET_MIN),
                ("max", REQ_GET_MAX),
                ("def", REQ_GET_DEF),
                ("step", REQ_GET_RES),
            ] {
                if let Some(v) = field(req) {
                    let v = if name == "step" {
                        v
                    } else {
                        uvc::display_value(def, v)
                    };
                    props.push(format!("\"{name}\": {v}"));
                }
            }
            if !first {
                out.push_str(",\n");
            }
            first = false;
            out.push_str(&format!("  \"{}\": {{{}}}", def.name, props.join(", ")));
        }
        out.push_str("\n}\n");
        Ok(out)
    }
}
