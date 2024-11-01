use powdr::Session;

fn main() {
    env_logger::init();

    let data: Vec<u32> = (0..1000).collect();

    let mut session = Session::builder()
        .guest_path("./guest")
        .out_path("powdr-target")
        .chunk_size_log2(18)
        .build()
        .write(1, &data)
        .write(2, &data.iter().sum::<u32>());

    // Fast dry run to test execution.
    session.run();

    session.prove();
}
