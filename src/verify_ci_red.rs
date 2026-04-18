#[allow(dead_code)]
pub fn bad() {
    let _x: i32 = 1;
}

#[cfg(test)]
mod tests {
    #[test]
    fn always_fails() {
        assert!(false);
    }
}
