pub fn pe_fixture(machine: u16, size: usize) -> Vec<u8> {
    assert!(size >= 640);
    let mut bytes = vec![0; size];
    bytes[..2].copy_from_slice(b"MZ");
    bytes[0x3c..0x40].copy_from_slice(&128u32.to_le_bytes());
    bytes[128..132].copy_from_slice(b"PE\0\0");
    bytes[132..134].copy_from_slice(&machine.to_le_bytes());
    bytes[134..136].copy_from_slice(&1u16.to_le_bytes());
    bytes[148..150].copy_from_slice(&240u16.to_le_bytes());
    bytes[152..154].copy_from_slice(&0x20bu16.to_le_bytes());
    for offset in (4096..size.saturating_sub(4)).step_by(5) {
        bytes[offset] = 0xe8;
        let relative = 0x20_0000i32.wrapping_sub(offset as i32 + 5);
        bytes[offset + 1..offset + 5].copy_from_slice(&relative.to_le_bytes());
    }
    bytes
}
