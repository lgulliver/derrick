#![allow(clippy::print_stdout)]
#![allow(clippy::print_stderr)]

use std::io::{self, Read};

use anyhow::{Context, Result};
use derrick_caveman::{Intensity, compress};

use crate::commands::{CavemanArgs, CavemanIntensity};

fn map_intensity(i: CavemanIntensity) -> Intensity {
    match i {
        CavemanIntensity::Lite => Intensity::Lite,
        CavemanIntensity::Full => Intensity::Full,
        CavemanIntensity::Ultra => Intensity::Ultra,
    }
}

pub(crate) async fn run(args: CavemanArgs) -> Result<()> {
    let mut raw = Vec::new();
    io::stdin().lock().read_to_end(&mut raw)?;
    let input = String::from_utf8(raw).context("caveman requires valid UTF-8 input")?;

    let intensity = map_intensity(args.intensity);
    let output = compress(&input, intensity);

    print!("{}", output.text);

    if args.stats {
        eprintln!(
            "caveman [{:?}]: {} \u{2192} {} chars ({:.1}% saved)",
            args.intensity,
            output.stats.chars_in,
            output.stats.chars_out,
            output.stats.savings_pct(),
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use derrick_caveman::{Intensity, compress};

    use super::*;

    #[test]
    fn caveman_lite_reduces_filler() {
        let input = "I would like to just basically mention that we should probably \
                     consider looking into this issue at some point in time.";
        let output = compress(input, Intensity::Lite);
        assert!(
            output.stats.chars_out < output.stats.chars_in,
            "expected compression: in={} out={}",
            output.stats.chars_in,
            output.stats.chars_out
        );
    }

    #[test]
    fn caveman_intensity_mapping() {
        assert_eq!(map_intensity(CavemanIntensity::Lite), Intensity::Lite);
        assert_eq!(map_intensity(CavemanIntensity::Full), Intensity::Full);
        assert_eq!(map_intensity(CavemanIntensity::Ultra), Intensity::Ultra);
    }
}
