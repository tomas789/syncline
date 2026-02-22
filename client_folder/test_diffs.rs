use dissimilar::diff;

fn main() {
    let diffs = diff("abc", "adc");
    for d in diffs {
        println!("{:?}", d);
    }
}
