use pcg_rand::Pcg64;
use rand::{Rng, SeedableRng};

// retrieve latest Windows Chrome user-agent from
// https://www.whatismybrowser.com/guides/the-latest-user-agent/chrome
pub(crate) const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/93.0.4577.63 Safari/537.36";

pub(crate) fn get_rng() -> impl Rng {
    Pcg64::from_entropy()
}
