// SPDX-License-Identifier: MPL-2.0

use crate::signaller::Signallable;
use gst::glib;

mod imp;

glib::wrapper! {
    pub struct CloudflareCallsProducerSignaller(ObjectSubclass<imp::CallsClient>) @implements Signallable;
}

impl Default for CloudflareCallsProducerSignaller {
    fn default() -> Self {
        glib::Object::builder().build()
    }
}
