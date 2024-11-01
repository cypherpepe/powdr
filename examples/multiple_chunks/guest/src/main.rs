use powdr_riscv_runtime;
use powdr_riscv_runtime::io::read;

fn main() {
    let list: Vec<u32> = read(1);
    let sum: u32 = read(2);

    assert_eq!(sum, list.iter().sum::<u32>());
}
