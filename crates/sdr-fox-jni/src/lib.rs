//! Android JNI bindings for sdr-fox. Built with `cargo ndk` for
//! `aarch64-linux-android` and `x86_64-linux-android`. Streaming uses
//! synchronous pulls into caller-owned direct byte buffers; native worker
//! threads never enter the JVM.

#![warn(missing_docs)]
#![warn(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]

mod jni_impl;
