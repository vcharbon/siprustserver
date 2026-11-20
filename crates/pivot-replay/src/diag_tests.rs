use super::*;

/// Two cells on two threads each keep their own lines, and a line written
/// outside any capture is kept by none.
#[test]
fn each_cell_keeps_the_lines_it_wrote_and_no_other() {
    line("before any cell");
    let other = std::thread::spawn(|| captured(|| line("cell two")).1);
    let ((), mine) = captured(|| {
        line("cell one: rfc finding");
        line(format_args!("cell one: {}", 2));
    });
    assert_eq!(mine, "cell one: rfc finding\ncell one: 2\n");
    assert_eq!(other.join().unwrap(), "cell two\n");
    let ((), after) = captured(|| ());
    assert_eq!(after, "", "a new capture starts empty");
}
