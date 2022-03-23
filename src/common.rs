use rand::Rng;

// retrieve latest Windows Firefox user-agent from
// https://www.whatismybrowser.com/guides/the-latest-user-agent/firefox
pub(crate) const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:98.0) \
Gecko/20100101 Firefox/98.0";

pub(crate) fn get_rng() -> impl Rng {
    rand::thread_rng()
}
