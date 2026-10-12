#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| keel_vfs::fuzz::recover_ntfs(data));
