//! 把一帧 EXR 解成裸 f32，供离线工具（渲染、统计）消费。
//!
//! ```text
//! cargo run -p depth-lab --features exr-out --example exr_to_f32 -- <in.exr> <out.f32>
//! ```
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 2 {
        eprintln!("用法: exr_to_f32 <in.exr> <out.f32>");
        std::process::exit(2);
    }
    let (d, w, h) = depth_lab::exr_out::read_frame(std::path::Path::new(&a[0])).unwrap();
    let mut bytes = Vec::with_capacity(d.len() * 4);
    for v in &d {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(&a[1], bytes).unwrap();
    println!("{}x{} → {}", w, h, a[1]);
}
