use sqlx::types::Uuid;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex, OnceCell};

use crate::queries::{Session, User};
use crate::router::{LeaderboardEntry, PlayerCtx, Response};
use crate::ToPlayerMgr;

pub struct Player {
    pub tx: UnboundedSender<ToPlayer>,
    tx_mgr: UnboundedSender<ToPlayerMgr>,
    rx: Mutex<UnboundedReceiver<ToPlayer>>,
    queue_rx: Mutex<UnboundedReceiver<Response>>,
    pub queue_tx: UnboundedSender<Response>,
    // stream: Arc<Mutex<TcpStream>>,
    has_shutdown: OnceCell<bool>,
    session: OnceCell<LoginSession>,
    context: Mutex<Option<PlayerCtx>>,
    context_id: Mutex<Option<Uuid>>,
    ip_address: String,
}

pub enum ToPlayer {
    NotifyRecord(),
    Top3(Vec<LeaderboardEntry>),
    Shutdown(),
    Send(Response),
}

#[derive(Debug, Clone)]
pub struct LoginSession {
    pub user: User,
    pub session: Session,
    pub resumed: bool,
}
impl LoginSession {
    pub fn display_name(&self) -> &str {
        &self.user.display_name
    }
    pub fn user_id(&self) -> &Uuid {
        &self.user.web_services_user_id
    }
    pub fn session_id(&self) -> &Uuid {
        &self.session.session_token
    }
}

/// Context stuff

const CONTEXT_COEFFICIENTS: [i32; 15] = [69, 59, 136, 1, 26, 77, 41, 1, 95, 53, 1, 1, 86, 62, 89];
// from shuffled to plain order
const CONTEXT_MAP_CYPHER_TO_PLAIN: [i32; 15] = [6, 1, 14, -1, 12, 0, 8, -1, 13, 5, -1, -1, 2, 4, 9];
const CONTEXT_MAP_PLAIN_TO_CYPHER: [i32; 15] = [5, 1, 12, -1, 13, 9, 0, -1, 6, 14, -1, -1, 4, 8, 2];

pub fn context_map_cypher_to_plain(cypher: i32) -> i32 {
    let ret = CONTEXT_MAP_CYPHER_TO_PLAIN[cypher as usize];
    if ret == -1 {
        // panic!("invalid index: {}", cypher);
        return cypher;
    }
    ret
}

pub fn context_map_plain_to_cypher(plain: i32) -> i32 {
    let ret = CONTEXT_MAP_PLAIN_TO_CYPHER[plain as usize];
    if ret == -1 {
        // panic!("invalid index: {}", plain);
        return plain;
    }
    ret
}

// return true if not in editor
pub fn check_flags_sf_mi(sf: u64, mi: u64) -> bool {
    let sfs = decode_context_flags(sf);
    if sfs[5] || sfs[7] || sfs[9] || sfs[11] {
        return false;
    }
    sfs[2] || sfs[3] || sfs[4]
}

/*

    // !
    const int[] unsafeMap = {5, 1, 12, -1, 13, 9, 0, -1, 6, 14, -1, -1, 4, 8, 2};
    bool[]@ unsafeEncodeShuffle(const array<bool>@ arr) {
        bool[] ret = array<bool>(15);
        for (uint i = 0; i < 15; i++) {
            if (unsafeMap[i] == -1) {
                ret[i] = arr[i];
            } else {
                ret[unsafeMap[i]] = arr[i];
            }
        }
        return ret;
    }
    uint64 encode(const array<bool>@ arr) {
        uint64 ret = 1;
        for (uint i = 0; i < 15; i++) {
            if (arr[i]) {
                ret *= uint64(lambda[i]);
            }
            while (ret & 1 == 0) {
                trace("trimming: " + Text::FormatPointer(ret));
                ret = ret >> 1;
            }
            trace("ret: " + Text::FormatPointer(ret));
        }
        return ret;
    }
    void runEncTest() {
        bool[] arr = {true, false, true, false, true, false, true, false, true, false, true, false, true, false, true};
        runEncTestCase(arr);
        for (uint i = 0; i < 15; i++) {
            arr[i] = true;
        }
        runEncTestCase(arr);
        for (uint i = 0; i < 15; i++) {
            arr[i] = i % 3 == 0;
        }
        runEncTestCase(arr);
        print("^ mod 3");
        for (uint i = 0; i < 15; i++) {
            arr[i] = i % 5 == 0;
        }
        runEncTestCase(arr);
        print("^ mod 5");
    }

    void runEncTestCase(const array<bool>@ arr) {
        auto sarr = unsafeEncodeShuffle(arr);
        trace('Encoded: ' + Text::FormatPointer(encode(sarr)));
        // auto enc = encode(arr);
        // trace("enc: " + Text::FormatPointer(enc));
        // auto dec = decode(enc);
        // trace("dec: " + dec);
        // auto enc2 = encode(dec);
        // trace("enc2: " + Text::FormatPointer(enc2));
        // auto dec2 = decode(enc2);
        // trace("dec2: " + dec2);
    }

*/

pub fn encode_context_flags(arr: &[bool; 15]) -> u64 {
    let mut ret = 1;
    for i in 0..15 {
        if arr[i] {
            ret *= CONTEXT_COEFFICIENTS[context_map_plain_to_cypher(i as i32) as usize] as u64;
        }
        while ret & 1 == 0 {
            ret = ret >> 1;
        }
    }
    ret
}

pub fn decode_context_flags(flags: u64) -> [bool; 15] {
    let mut plain = [false; 15];
    if flags == 0 {
        return plain;
    }
    let mut flags = flags;
    for i in 0..15 {
        if CONTEXT_COEFFICIENTS[i] < 2 {
            continue;
        }
        let mut c = CONTEXT_COEFFICIENTS[i] as u64;
        while c & 1 == 0 && c > 0 {
            c = c >> 1;
        }
        if flags % c == 0 {
            plain[context_map_cypher_to_plain(i as i32) as usize] = true;
            flags = flags / c;
        }
    }
    plain
}

pub fn parse_u64_str(s: &str) -> Result<u64, std::num::ParseIntError> {
    if &s[..2] == "0x" {
        return u64::from_str_radix(&s[2..], 16);
    }
    u64::from_str_radix(s, 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode() {
        let all_true: [bool; 15] = [true; 15];
        let enc = encode_context_flags(&all_true);
        // eprintln!("{:?}", enc);
        let plain = decode_context_flags(enc);
        // eprintln!("{:?}", plain);
        for i in 0..15 {
            if CONTEXT_MAP_CYPHER_TO_PLAIN[i] < 0 {
                continue;
            }
            assert_eq!(plain[i], true);
        }

        let all_false: [bool; 15] = [false; 15];
        let enc = encode_context_flags(&all_false);
        // eprintln!("{:?}", enc);
        let plain = decode_context_flags(enc);
        // eprintln!("{:?}", plain);
        for i in 0..15 {
            if CONTEXT_MAP_CYPHER_TO_PLAIN[i] < 0 {
                continue;
            }
            assert_eq!(plain[i], false);
        }

        let alternating: [bool; 15] = [
            true, false, true, false, true, false, true, false, true, false, true, false, true, false, true,
        ];
        let enc = encode_context_flags(&alternating);
        // eprintln!("{:?}", enc);
        let plain = decode_context_flags(enc);
        // eprintln!("{:?}", plain);
        for i in 0..15 {
            if CONTEXT_MAP_CYPHER_TO_PLAIN[i] < 0 {
                continue;
            }
            assert_eq!(plain[i], i % 2 == 0);
        }

        let mod3: [bool; 15] = [
            true, false, false, true, false, false, true, false, false, true, false, false, true, false, false,
        ];
        let enc = encode_context_flags(&mod3);
        let plain = decode_context_flags(enc);
        for i in 0..15 {
            if CONTEXT_MAP_CYPHER_TO_PLAIN[i] < 0 {
                continue;
            }
            assert_eq!(plain[i], i % 3 == 0);
        }

        let mod5: [bool; 15] = [
            true, false, false, false, false, true, false, false, false, false, true, false, false, false, false,
        ];
        let enc = encode_context_flags(&mod5);
        let plain = decode_context_flags(enc);
        for i in 0..15 {
            if CONTEXT_MAP_CYPHER_TO_PLAIN[i] < 0 {
                continue;
            }
            assert_eq!(plain[i], i % 5 == 0);
        }
    }

    #[test]
    fn test_decode() {
        let all_true: u64 = 0x178BA59576325029;
        let plain = decode_context_flags(all_true);
        for i in 0..15 {
            if CONTEXT_COEFFICIENTS[i] < 2 {
                assert_eq!(plain[i], false);
            } else {
                assert_eq!(plain[i], true);
                if plain[i] {
                    // eprintln!("{}: {}", i, CONTEXT_COEFFICIENTS[i]);
                }
            }
        }

        let alternating: u64 = 0x0000000EF0F42FA9;
        let plain = decode_context_flags(alternating);
        for i in 0..15 {
            if CONTEXT_COEFFICIENTS[i] < 2 {
                assert_eq!(plain[i], false);
            } else {
                assert_eq!(plain[i], i % 2 == 0);
                if plain[i] {
                    // eprintln!("{}: {}", i, CONTEXT_COEFFICIENTS[i]);
                }
            }
        }

        let mod3: u64 = 0x00000000005DCC45;
        let plain = decode_context_flags(mod3);
        for i in 0..15 {
            if CONTEXT_COEFFICIENTS[i] < 2 {
                assert_eq!(plain[i], false);
            } else {
                assert_eq!(plain[i], i % 3 == 0);
                if plain[i] {
                    // eprintln!("{}: {}", i, CONTEXT_COEFFICIENTS[i]);
                }
            }
        }

        let mod5: u64 = 0x0000000000000FF1;
        let plain = decode_context_flags(mod5);
        for i in 0..15 {
            if CONTEXT_COEFFICIENTS[i] < 2 {
                assert_eq!(plain[i], false);
            } else {
                assert_eq!(plain[i], i % 5 == 0);
                if plain[i] {
                    // eprintln!("{}: {}", i, CONTEXT_COEFFICIENTS[i]);
                }
            }
        }
    }

    #[test]
    fn test_context_map() {
        for i in 0..15 {
            let p2c = CONTEXT_MAP_PLAIN_TO_CYPHER[i];
            if (p2c) == -1 {
                continue;
            }
            let c2p = CONTEXT_MAP_CYPHER_TO_PLAIN[p2c as usize];
            assert_eq!(c2p, i as i32);
        }
    }

    #[test]
    fn test_context_map2() {
        for i in 0..15 {
            if i == 3 || i == 7 || i == 10 || i == 11 {
                continue;
            }
            assert_eq!(i, context_map_cypher_to_plain(context_map_plain_to_cypher(i)));
        }
    }
}
