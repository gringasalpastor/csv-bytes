pub fn two() -> u32 {
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn use_lookup_table() {
        assert_eq!(1 + 1, two());
    }
}
