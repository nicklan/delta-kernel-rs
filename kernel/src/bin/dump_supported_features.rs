use delta_kernel::table_features::{TableFeature, KernelSupport};

use strum::IntoEnumIterator;

pub fn main() {
    for feature in TableFeature::iter() {
        if let Some(info) = feature.info() {
            let read_supported = match info.read_support {
                KernelSupport::NotSupported => "No",
                KernelSupport::Custom(_) => "With Conditions",
                _ => "Yes",
            };
            let write_supported = match info.write_support {
                KernelSupport::NotSupported => "No",
                KernelSupport::Custom(_) => "With Conditions",
                _ => "Yes",
            };
            println!("{}\tRead: {}\tWrite: {}", info.name, read_supported, write_supported);
        }
    }
}
