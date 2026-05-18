use derrick_caveman::{compress, Intensity};

#[test]
fn caveman_full_hits_60_pct_on_verbose_prose() {
    let input = "\
I would like to let you know that in order to implement this solution, \
you should certainly make sure to basically just follow these steps. \
It is important to note that this approach is perhaps probably the best way \
to handle this. Actually, you should really just simply go ahead and start \
with the first step. Of course, you may need to perhaps consider various factors. \
Certainly, I would be happy to help explain further. Moreover, it is worth noting \
that this implementation will essentially address all the requirements. Furthermore, \
needless to say, the approach is generally clearly obviously the right one here. \
As you can see, due to the fact that we have made these changes, the results are \
consequently therefore generally typically expected to be positive.";
    let output = compress(input, Intensity::Full);
    assert!(
        output.stats.savings_pct() >= 60.0,
        "expected >=60% savings at Full, got {:.1}% (in={} out={})",
        output.stats.savings_pct(),
        output.stats.chars_in,
        output.stats.chars_out,
    );
}

#[test]
fn caveman_ultra_hits_60_pct_on_verbose_prose() {
    let input = "\
I would like to let you know that in order to implement this solution, \
you should certainly make sure to basically just follow these steps. \
It is important to note that this approach is perhaps probably the best way \
to handle this. Actually, you should really just simply go ahead and start \
with the first step. Of course, you may need to perhaps consider various factors. \
Certainly, I would be happy to help explain further. Moreover, it is worth noting \
that this implementation will essentially address all the requirements.";
    let output = compress(input, Intensity::Ultra);
    assert!(
        output.stats.savings_pct() >= 60.0,
        "expected >=60% savings at Ultra, got {:.1}% (in={} out={})",
        output.stats.savings_pct(),
        output.stats.chars_in,
        output.stats.chars_out,
    );
}
