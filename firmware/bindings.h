// Extra ESP-IDF / component headers exposed to Rust as `esp_idf_sys::usb::*`.
#include "usb/usb_host.h"
#include "usb/uvc_host.h"

// Exported by the usb_host_uvc component (esp_private/uvc_control.h, whose
// includes are not on the public include path): a control transfer on the
// camera's default endpoint, serialized with the driver's own requests.
esp_err_t uvc_host_usb_ctrl(uvc_host_stream_hdl_t stream_hdl, uint8_t bmRequestType, uint8_t bRequest, uint16_t wValue, uint16_t wIndex, uint16_t wLength, uint8_t *data);
