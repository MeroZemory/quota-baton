pub fn private_bytes() -> u64 {
    let Ok(contents) = std::fs::read_to_string("/proc/self/smaps_rollup") else {
        return 0;
    };
    contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            match fields.next()? {
                "Private_Clean:" | "Private_Dirty:" | "Private_Hugetlb:" => {
                    fields.next()?.parse::<u64>().ok()
                }
                _ => None,
            }
        })
        .sum::<u64>()
        .saturating_mul(1024)
}
