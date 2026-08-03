//! The fisherman's day.
//!
//! A scripted routine that loops, anchored to the **real sun** at your
//! location: he gets up before sunrise, works out, has coffee, gardens, fishes,
//! comes in for lunch, sleeps it off, fishes again, comes back an hour before
//! sunset, cooks dinner, reads, and goes to bed. In December he does all of it
//! later and has less of an afternoon, because in December the sun does.
//!
//! ## Why a script and not a shuffle
//!
//! The first version drew a random activity every four seconds. It produced
//! *variety* and no *life* — he did things in an order that meant nothing, and
//! after a minute of watching you could tell there was a dice roll behind it.
//! A person's day has shape: you eat lunch after the morning and before the
//! afternoon, you do not eat three lunches because the dice said so.
//!
//! So the day is a fixed sequence of blocks and the randomness lives inside
//! them — when exactly he breaks for a cigarette, how long a catch takes, which
//! way he is facing. Routine with texture, rather than texture with no routine.
//!
//! Everything here is a pure function of the clock and a per-day seed, which is
//! what lets a whole day be tested in a millisecond instead of watched for
//! sixteen hours.

/// Where he is.
///
/// Distinct from what he is doing, because the walk between two places is
/// generated rather than scripted — see [`day_plan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Place {
    /// Inside, visible through the window if there is light on him.
    Hut,
    /// Just outside the door. Where he exercises and drinks his coffee.
    Doorstep,
    /// The patch beside the hut.
    Garden,
    /// The cooking fire.
    Fire,
    /// The end of the rail, rod out.
    Perch,
}

/// What he is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Doing {
    Sleeping,
    /// Emerging, stretching the night off.
    Waking,
    Exercising,
    Coffee,
    Gardening,
    Fishing,
    Cooking,
    Eating,
    /// The afternoon lie-down.
    Siesta,
    Reading,
    /// Between two places. Inserted automatically, never scripted.
    Walking,
    /// A cigarette, taken roughly hourly out of whatever he was doing.
    Smoking,
}

impl Doing {
    /// Whether he is inside, and so only visible through the window.
    pub fn is_indoors(self, place: Place) -> bool {
        place == Place::Hut && self != Doing::Walking
    }

    /// Whether he would step out for a cigarette during this.
    ///
    /// Not while asleep, not mid-walk, not while eating, and not while already
    /// smoking — the rest is fair game.
    pub fn takes_a_break(self) -> bool {
        matches!(
            self,
            Doing::Fishing | Doing::Gardening | Doing::Coffee | Doing::Reading
        )
    }
}

/// One stretch of the day.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Block {
    pub doing: Doing,
    pub place: Place,
    /// Hours since local midnight.
    pub start: f64,
    pub end: f64,
}

impl Block {
    pub fn contains(&self, hours: f64) -> bool {
        hours >= self.start && hours < self.end
    }

    /// How far through it, 0 to 1.
    pub fn progress(&self, hours: f64) -> f64 {
        if self.end <= self.start {
            return 0.0;
        }
        ((hours - self.start) / (self.end - self.start)).clamp(0.0, 1.0)
    }
}

/// How long he takes to walk between two places.
///
/// Twelve seconds, not the minute and a half this used to be. That worked out
/// at **one step every eighteen seconds** — 3.3 steps a minute against a
/// documented human range of 80 to 120 — which is not a saunter, it is a man
/// wading through setting concrete. Nothing in the code said so; the number had
/// to be divided out to see it.
///
/// Milt Kahl timed pedestrians on his first week at Disney and found them
/// "invariably on twelve exposures — right on the nose", which at 24fps is half
/// a second a step, 120 steps a minute. Williams' own slow chart is ⅔ of a
/// second, 90 a minute, and that is the saunter. At 5fps the honest choices are
/// three frames a step (600ms, 100/min, mid-range) or four (800ms, 75/min, just
/// under the documented floor). Three.
pub const WALK_SECONDS: f64 = 12.0;
const WALK_MINUTES: f64 = WALK_SECONDS / 60.0;

/// Seconds per step, from Kahl's stopwatch by way of Williams' timing charts.
pub const STEP_SECONDS: f64 = 0.6;
/// A cigarette, and how far either side of the hour he takes it.
const SMOKE_MINUTES: f64 = 3.0;
const SMOKE_JITTER_MINUTES: f64 = 22.0;

/// The whole day, in order, covering every hour with no gaps.
///
/// `sunrise` and `sunset` are hours since local midnight. Blocks are scheduled
/// against them rather than against the clock, so the routine breathes with the
/// season — a June morning gives him three hours of gardening and fishing
/// before lunch where December gives him one.
///
/// Sleep is split around midnight rather than wrapping, so every block lies in
/// `0.0..24.0` and "the day is contiguous" is a property that can simply be
/// checked.
pub fn day_plan(sunrise: f64, sunset: f64) -> Vec<Block> {
    let hour = 1.0;
    let minutes = |m: f64| m / 60.0;

    // Noon is taken as the midpoint of the day rather than as twelve o'clock,
    // so lunch sits in the middle of *his* day and not the clock's.
    let noon = (sunrise + sunset) / 2.0;
    let up = sunrise - minutes(30.0);
    let bed = (sunset + 2.0 * hour).min(23.5);

    let scripted = [
        (Doing::Waking, Place::Hut, up),
        (Doing::Exercising, Place::Doorstep, up + minutes(10.0)),
        (Doing::Coffee, Place::Doorstep, sunrise + minutes(15.0)),
        (Doing::Gardening, Place::Garden, sunrise + minutes(45.0)),
        (
            Doing::Fishing,
            Place::Perch,
            sunrise + 2.0 * hour + minutes(15.0),
        ),
        (Doing::Cooking, Place::Fire, noon - minutes(45.0)),
        (Doing::Eating, Place::Doorstep, noon - minutes(10.0)),
        (Doing::Siesta, Place::Hut, noon + minutes(20.0)),
        (Doing::Fishing, Place::Perch, noon + 2.0 * hour),
        (Doing::Cooking, Place::Fire, sunset - hour),
        (Doing::Eating, Place::Doorstep, sunset - minutes(25.0)),
        (Doing::Reading, Place::Hut, sunset + minutes(10.0)),
        (Doing::Sleeping, Place::Hut, bed),
    ];

    // Each scripted block runs until the next one starts.
    let mut blocks = Vec::with_capacity(scripted.len() * 2 + 2);
    // The tail of last night.
    blocks.push(Block {
        doing: Doing::Sleeping,
        place: Place::Hut,
        start: 0.0,
        end: up,
    });

    for (index, (doing, place, start)) in scripted.iter().enumerate() {
        let end = scripted
            .get(index + 1)
            .map(|(_, _, next)| *next)
            .unwrap_or(24.0);
        blocks.push(Block {
            doing: *doing,
            place: *place,
            start: *start,
            end,
        });
    }

    insert_walks(blocks)
}

/// Put a walk at the front of every block that changes place.
///
/// Generated rather than scripted, so adding an activity to the day above needs
/// no thought about how he gets there — and so a walk can never be forgotten,
/// which is the failure that would have him teleport.
fn insert_walks(blocks: Vec<Block>) -> Vec<Block> {
    let walk = WALK_MINUTES / 60.0;
    let mut out: Vec<Block> = Vec::with_capacity(blocks.len() * 2);

    for block in blocks {
        let changed_place = out
            .last()
            .is_some_and(|last: &Block| last.place != block.place);
        // Only if the block is long enough to spare the time, so a squeezed
        // winter afternoon cannot produce a negative-length activity.
        if changed_place && block.end - block.start > walk * 2.0 {
            out.push(Block {
                doing: Doing::Walking,
                place: block.place,
                start: block.start,
                end: block.start + walk,
            });
            out.push(Block {
                start: block.start + walk,
                ..block
            });
        } else {
            out.push(block);
        }
    }

    out
}

/// What he is doing at a moment, and how far through it he is.
///
/// The cigarette is layered over the scripted day rather than scheduled in it:
/// he does not *plan* to smoke at 11:14, he is gardening and stops for one.
pub fn at(hours: f64, sunrise: f64, sunset: f64, day: i64) -> (Block, f64) {
    let plan = day_plan(sunrise, sunset);
    let block = plan
        .iter()
        .find(|block| block.contains(hours))
        .copied()
        // Past the end of the last block only by a rounding error; the day ends
        // asleep either way.
        .unwrap_or(Block {
            doing: Doing::Sleeping,
            place: Place::Hut,
            start: 23.5,
            end: 24.0,
        });

    if block.doing.takes_a_break() {
        if let Some(lit) = cigarette_at(hours, day) {
            return (
                Block {
                    doing: Doing::Smoking,
                    ..block
                },
                lit,
            );
        }
    }

    let progress = block.progress(hours);
    (block, progress)
}

/// Whether he is mid-cigarette, and how far through it.
///
/// Roughly once an hour, moved by up to twenty minutes either way on a seed
/// made from the hour and the day — so he smokes about hourly and never on the
/// hour, and the same day always plays out the same way.
fn cigarette_at(hours: f64, day: i64) -> Option<f64> {
    // The jitter keeps every cigarette inside its own hour — lit at
    // half-past ± twenty-two minutes and out three minutes later — so only
    // the current hour can ever be burning.
    let hour = hours.floor();
    let jitter = crate::fisherman::unit_from(hour as i64, day) * 2.0 - 1.0;
    let lit = hour + 0.5 + jitter * (SMOKE_JITTER_MINUTES / 60.0);
    let out = lit + SMOKE_MINUTES / 60.0;
    if hours >= lit && hours < out {
        return Some((hours - lit) / (SMOKE_MINUTES / 60.0));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A summer day and a winter day in San Francisco, roughly.
    const SUMMER: (f64, f64) = (5.8, 20.6);
    const WINTER: (f64, f64) = (7.4, 16.9);

    /// **The day must have no holes in it.** A gap is a moment with nothing to
    /// draw, and an overlap is two activities claiming the same second — the
    /// first shows as a flicker, the second as whichever block the search
    /// happened to reach first.
    #[test]
    fn the_day_is_covered_end_to_end_with_no_gaps_and_no_overlaps() {
        for (sunrise, sunset) in [SUMMER, WINTER, (4.5, 22.0), (8.0, 16.0)] {
            let plan = day_plan(sunrise, sunset);
            assert_eq!(
                plan.first().unwrap().start,
                0.0,
                "the day starts at midnight"
            );
            assert_eq!(plan.last().unwrap().end, 24.0, "and runs to the next one");

            for pair in plan.windows(2) {
                assert!(
                    (pair[0].end - pair[1].start).abs() < 1e-9,
                    "sunrise {sunrise}: {:?} ends at {:.4} but {:?} starts at {:.4}",
                    pair[0].doing,
                    pair[0].end,
                    pair[1].doing,
                    pair[1].start
                );
            }
            for block in &plan {
                assert!(
                    block.end > block.start,
                    "sunrise {sunrise}: {:?} has no duration",
                    block.doing
                );
            }
        }
    }

    /// The routine as a person would describe it, in order. This is the brief
    /// made executable — if the day stops making sense, this is what says so.
    #[test]
    fn he_lives_a_day_that_makes_sense() {
        let (sunrise, sunset) = SUMMER;
        let doing = |hours: f64| at(hours, sunrise, sunset, 19_000).0.doing;

        assert_eq!(doing(3.0), Doing::Sleeping, "the small hours");
        assert_eq!(doing(sunrise - 0.4), Doing::Waking, "up before the sun");
        assert_eq!(doing(sunrise - 0.1), Doing::Exercising, "a workout first");
        assert_eq!(doing(sunrise + 0.4), Doing::Coffee, "then coffee");
        assert_eq!(doing(sunrise + 1.5), Doing::Gardening, "then the garden");
        assert_eq!(doing(sunrise + 3.5), Doing::Fishing, "then the rod");

        let noon = (sunrise + sunset) / 2.0;
        assert_eq!(doing(noon - 0.5), Doing::Cooking, "lunch is cooked");
        assert_eq!(doing(noon), Doing::Eating, "and eaten");
        assert_eq!(doing(noon + 1.0), Doing::Siesta, "then a lie-down");
        assert_eq!(doing(noon + 3.0), Doing::Fishing, "back to the water");

        assert_eq!(
            doing(sunset - 0.7),
            Doing::Cooking,
            "dinner an hour before dark"
        );
        assert_eq!(doing(sunset - 0.2), Doing::Eating);
        assert_eq!(doing(sunset + 1.0), Doing::Reading, "a book after dark");
        assert_eq!(doing(23.9), Doing::Sleeping, "and bed");
    }

    /// The whole point of anchoring to the sun. A winter day has to be a
    /// *shorter working day*, not the same day shifted — he gets up later and
    /// has less of an afternoon, because there is less afternoon.
    #[test]
    fn the_routine_breathes_with_the_season() {
        let outdoor_hours = |(sunrise, sunset): (f64, f64)| {
            day_plan(sunrise, sunset)
                .iter()
                .filter(|b| b.place == Place::Perch)
                .map(|b| b.end - b.start)
                .sum::<f64>()
        };

        let summer = outdoor_hours(SUMMER);
        let winter = outdoor_hours(WINTER);
        assert!(
            summer > winter + 2.0,
            "summer gives {summer:.1}h at the water and winter {winter:.1}h — \
             too close for the season to be doing anything"
        );

        // And he rises with the sun rather than with the clock.
        let waking = |(sunrise, sunset): (f64, f64)| {
            day_plan(sunrise, sunset)
                .iter()
                .find(|b| b.doing == Doing::Waking)
                .map(|b| b.start)
                .expect("he gets up")
        };
        assert!(waking(WINTER) > waking(SUMMER) + 1.0, "a winter lie-in");
    }

    /// He never teleports. Every change of place has a walk in front of it, so
    /// adding an activity to the script needs no thought about how he gets
    /// there — and a walk cannot be forgotten.
    #[test]
    fn he_walks_between_places_rather_than_appearing_at_them() {
        for (sunrise, sunset) in [SUMMER, WINTER] {
            let plan = day_plan(sunrise, sunset);
            for pair in plan.windows(2) {
                if pair[0].place == pair[1].place {
                    continue;
                }
                assert_eq!(
                    pair[1].doing,
                    Doing::Walking,
                    "he jumped from {:?} at {:?} to {:?} at {:?}",
                    pair[0].place,
                    pair[0].doing,
                    pair[1].place,
                    pair[1].doing
                );
            }
        }
    }

    /// About one an hour, never on the hour, and never while asleep or eating.
    /// "Roughly hourly" is a number, so it is a test.
    #[test]
    fn he_smokes_about_once_an_hour_and_never_at_the_wrong_moment() {
        let (sunrise, sunset) = SUMMER;
        let day = 19_042;
        let mut smokes = 0;
        let mut previous = Doing::Sleeping;

        // Every ten seconds through the whole day.
        for step in 0..(24 * 360) {
            let hours = f64::from(step) / 360.0;
            let (block, _) = at(hours, sunrise, sunset, day);
            if block.doing == Doing::Smoking && previous != Doing::Smoking {
                smokes += 1;
            }
            if block.doing == Doing::Smoking {
                assert_ne!(block.place, Place::Fire, "not over the cooking");
            }
            previous = block.doing;
        }

        // He is awake for about sixteen hours and smokes during roughly half of
        // his activities, so a dozen or so.
        assert!(
            (6..=18).contains(&smokes),
            "{smokes} cigarettes in a day is not 'about hourly'"
        );
    }

    /// Same day, same routine. Two runs of the same afternoon must not differ,
    /// or he would change his mind while being watched.
    #[test]
    fn a_day_plays_out_the_same_way_every_time() {
        let (sunrise, sunset) = SUMMER;
        for step in 0..500 {
            let hours = f64::from(step) / 20.0;
            assert_eq!(
                at(hours, sunrise, sunset, 19_003),
                at(hours, sunrise, sunset, 19_003)
            );
        }
        // And a different day differs somewhere — the texture is per-day.
        let differs = (0..24 * 60).any(|m| {
            let hours = f64::from(m) / 60.0;
            at(hours, sunrise, sunset, 19_003) != at(hours, sunrise, sunset, 19_004)
        });
        assert!(
            differs,
            "every day is identical — the seed is doing nothing"
        );
    }
}
