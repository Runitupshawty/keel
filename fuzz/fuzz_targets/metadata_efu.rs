#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| keel_search::fuzz::metadata_efu(data));
