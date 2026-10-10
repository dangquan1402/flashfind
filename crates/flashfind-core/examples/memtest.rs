//! Does freed heap leave the process footprint? (allocator diagnostics)
fn main() {
    let f = flashfind_core::walk::footprint_mb;
    println!("start {:.1}", f());
    let mut small: Vec<Vec<u8>> = (0..551_000).map(|i| vec![(i % 251) as u8; 60 + i % 60]).collect();
    let big = vec![1u8; 35 << 20];
    println!("after alloc {:.1}", f());
    small.clear();
    small.shrink_to_fit();
    println!("after free small {:.1}", f());
    drop(big);
    println!("after free big {:.1}", f());
    flashfind_core::release_memory();
    println!("after release {:.1}", f());
    std::thread::sleep(std::time::Duration::from_millis(500));
    println!("after 0.5 s {:.1}", f());
}
