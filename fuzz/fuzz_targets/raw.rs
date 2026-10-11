#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| keel_core::fuzz::raw(data));
