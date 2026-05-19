pub fn scanner_frames() -> Vec<String> {
    const WIDTH: usize = 12;
    let mut frames = Vec::with_capacity(WIDTH * 2 - 2);
    for i in 0..WIDTH {
        let mut s = vec![' '; WIDTH];
        s[i] = '\u{2593}';
        frames.push(s.into_iter().collect());
    }
    for i in (1..WIDTH - 1).rev() {
        let mut s = vec![' '; WIDTH];
        s[i] = '\u{2593}';
        frames.push(s.into_iter().collect());
    }
    frames
}
