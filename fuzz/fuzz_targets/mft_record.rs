#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| keel_search::fuzz::mft_record(data));
