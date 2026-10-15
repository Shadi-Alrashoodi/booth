// Lengths and capacities of the collections that live as long as a room or
// the app, for finding the one that keeps growing in a long session. Only a
// build with the memstats feature keeps them: without it ON is false and
// every gauge! is dead code, though it still has to compile.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

pub const ON: bool = cfg!(feature = "memstats");

// An owner sets its gauges at least this often while it lives. One not set
// for longer belongs to something that is gone, and drops out of the totals.
pub const STALE_AFTER: Duration = Duration::from_secs(90);

pub trait Measured {
    fn length(&self) -> usize;
    fn cap(&self) -> usize;
}

impl<T> Measured for Vec<T> {
    fn length(&self) -> usize {
        Vec::len(self)
    }
    fn cap(&self) -> usize {
        self.capacity()
    }
}

impl<T> Measured for VecDeque<T> {
    fn length(&self) -> usize {
        VecDeque::len(self)
    }
    fn cap(&self) -> usize {
        self.capacity()
    }
}

impl<K, V, S> Measured for HashMap<K, V, S> {
    fn length(&self) -> usize {
        HashMap::len(self)
    }
    fn cap(&self) -> usize {
        self.capacity()
    }
}

impl<T, S> Measured for HashSet<T, S> {
    fn length(&self) -> usize {
        HashSet::len(self)
    }
    fn cap(&self) -> usize {
        self.capacity()
    }
}

// A B-tree has no spare capacity to speak of: its nodes come and go with
// its entries.
impl<K, V> Measured for BTreeMap<K, V> {
    fn length(&self) -> usize {
        BTreeMap::len(self)
    }
    fn cap(&self) -> usize {
        BTreeMap::len(self)
    }
}

impl<T> Measured for BTreeSet<T> {
    fn length(&self) -> usize {
        BTreeSet::len(self)
    }
    fn cap(&self) -> usize {
        BTreeSet::len(self)
    }
}

impl Measured for String {
    fn length(&self) -> usize {
        String::len(self)
    }
    fn cap(&self) -> usize {
        self.capacity()
    }
}

// One name summed over every live collection set under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Total {
    pub name: &'static str,
    pub instances: usize,
    pub len: usize,
    pub cap: usize,
    pub largest: usize,
}

struct Reading {
    len: usize,
    cap: usize,
    at: Instant,
}

#[derive(Default)]
pub struct Registry {
    readings: Mutex<BTreeMap<(&'static str, usize), Reading>>,
}

impl Registry {
    pub const fn new() -> Registry {
        Registry {
            readings: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn set(&self, name: &'static str, key: usize, len: usize, cap: usize, now: Instant) {
        let mut readings = self.readings.lock().unwrap_or_else(PoisonError::into_inner);
        readings.insert((name, key), Reading { len, cap, at: now });
    }

    pub fn totals(&self, now: Instant) -> Vec<Total> {
        let mut readings = self.readings.lock().unwrap_or_else(PoisonError::into_inner);
        readings.retain(|_, reading| now.saturating_duration_since(reading.at) <= STALE_AFTER);
        let mut totals: Vec<Total> = Vec::new();
        for ((name, _), reading) in readings.iter() {
            match totals.last_mut() {
                Some(total) if total.name == *name => {
                    total.instances += 1;
                    total.len += reading.len;
                    total.cap += reading.cap;
                    total.largest = total.largest.max(reading.len);
                }
                _ => totals.push(Total {
                    name,
                    instances: 1,
                    len: reading.len,
                    cap: reading.cap,
                    largest: reading.len,
                }),
            }
        }
        totals
    }
}

static GAUGES: Registry = Registry::new();

pub fn set(name: &'static str, key: usize, len: usize, cap: usize) {
    if ON {
        GAUGES.set(name, key, len, cap, Instant::now());
    }
}

pub fn totals() -> Vec<Total> {
    GAUGES.totals(Instant::now())
}

// gauge!("room.host.peers", self.peers) sets one collection, keyed by where
// it sits. gauge!("room.host.peer_sends", self, len, cap) sets a sum the
// owner adds up over what it holds, keyed by the owner, for collections
// that sit inside the elements of another and move when it grows.
#[macro_export]
macro_rules! gauge {
    ($name:expr, $collection:expr) => {
        if $crate::mem::ON {
            let collection = &$collection;
            $crate::mem::set(
                $name,
                ::std::ptr::from_ref(collection).cast::<()>() as usize,
                $crate::mem::Measured::length(collection),
                $crate::mem::Measured::cap(collection),
            );
        }
    };
    ($name:expr, $owner:expr, $len:expr, $cap:expr) => {
        if $crate::mem::ON {
            $crate::mem::set(
                $name,
                ::std::ptr::from_ref($owner).cast::<()>() as usize,
                $len,
                $cap,
            );
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_add_up_each_name_over_its_instances() {
        let registry = Registry::new();
        let now = Instant::now();
        registry.set("a", 1, 3, 4, now);
        registry.set("a", 2, 5, 8, now);
        registry.set("b", 1, 0, 16, now);
        registry.set("a", 1, 4, 4, now);
        assert_eq!(
            registry.totals(now),
            vec![
                Total {
                    name: "a",
                    instances: 2,
                    len: 9,
                    cap: 12,
                    largest: 5,
                },
                Total {
                    name: "b",
                    instances: 1,
                    len: 0,
                    cap: 16,
                    largest: 0,
                },
            ]
        );
    }

    #[test]
    fn a_reading_not_set_again_drops_out() {
        let registry = Registry::new();
        let then = Instant::now();
        registry.set("gone", 1, 7, 8, then);
        registry.set("kept", 1, 1, 1, then + STALE_AFTER);
        let totals = registry.totals(then + STALE_AFTER + Duration::from_secs(1));
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].name, "kept");
    }

    #[test]
    fn the_macro_measures_what_it_is_given() {
        let peers = vec![1u8, 2, 3];
        let names: HashMap<u8, String> = HashMap::new();
        crate::gauge!("test.peers", peers);
        crate::gauge!("test.names", &names, names.len(), names.capacity());
    }
}
