#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| outpace_fuzz::peer_frames(data));
