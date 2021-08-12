pub fn iter_bits(mut v: u32) -> impl Iterator<Item=usize> {
    let mut index = 0;
    std::iter::from_fn(move || {
        if v == 0 {
            None
        } else {
            let shift = v.trailing_zeros();
            v >>= shift + 1;
            let res = index + shift;
            index += shift + 1;
            Some(res as usize)
        }
    })
}
