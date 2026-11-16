//! The stream's rate and timing rules (`rate`), on a made-up clock: what the first live
//! runs called for. Nothing here waits or sends.

use std::time::{Duration, Instant};

use super::super::rate::{Ask, AudioClock, Budget, EvenClock, KeyframeGate, Pacer, MIN_WIRE};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Runs a budget for `seconds` as the worker does, every 20 ms: the encoder gives `gain`
/// times what it was last asked for (packet overhead, overshoot and keyframes in one
/// number). Returns the time it ended and how often the rate asked of the encoder changed.
fn run(budget: &mut Budget, from: Instant, seconds: f64, gain: impl Fn(u32) -> f64) -> (Instant, u32) {
    let steps = (seconds * 50.0).round() as u64;
    let (mut now, mut changes) = (from, 0);
    for _ in 0..steps {
        now += ms(20);
        budget.on_sent((gain(budget.asked()) * budget.asked() as f64 / 8.0 / 50.0) as usize);
        budget.update(now);
        changes += (budget.ask(now).1 != Ask::Same) as u32;
    }
    (now, changes)
}

#[test]
fn golive_rate_starts_at_what_was_announced() {
    let t0 = Instant::now();
    let mut budget = Budget::new(8_000_000, true, false, t0);
    // 88 % of the wire rate is asked of the encoder: packets add about 7 % to its bytes.
    assert_eq!((budget.wire(), budget.encoder(), budget.asked(), budget.encoder_share()), (8_000_000, 7_040_000, 7_040_000, 88));
    assert_eq!(budget.ask(t0 + ms(10)), (7_040_000, Ask::Same));
    // With stream audio a tenth of a megabit is the audio's.
    assert_eq!(Budget::new(8_000_000, true, true, t0).asked(), 6_952_000);
    // A low maximum is kept as it is; one below the floor is the floor.
    assert_eq!(Budget::new(3_000_000, true, false, t0).wire(), 3_000_000);
    assert_eq!(Budget::new(100_000, true, false, t0).wire(), MIN_WIRE);
}

/// An estimate that has stayed where it is for a second (two reports, a second apart).
fn settled(budget: &mut Budget, estimate: u64, at: Instant) -> Instant {
    budget.on_remb(estimate, at);
    budget.on_remb(estimate, at + ms(1000));
    at + ms(1000)
}

#[test]
fn golive_rate_follows_the_estimate_down_within_a_second_and_up_slowly() {
    let t0 = Instant::now();
    let mut budget = Budget::new(8_000_000, true, false, t0);
    // What the first live run saw when more viewers joined: 9.0, then 2.8.
    budget.on_remb(9_000_000, t0 + ms(100));
    assert_eq!(budget.wire(), 8_000_000, "never above what was announced");
    // One low report is not followed yet (the third live run: 1.9, 6.8, 1.9, 6.9 in four
    // lines, without a lost packet).
    budget.on_remb(2_800_000, t0 + ms(200));
    budget.on_remb(2_800_000, t0 + ms(900));
    assert_eq!(budget.wire(), 8_000_000);
    budget.on_remb(9_000_000, t0 + ms(1000));
    budget.on_remb(2_800_000, t0 + ms(1500));
    budget.update(t0 + ms(2400));
    assert_eq!(budget.wire(), 8_000_000, "the second starts over when the estimate was back in between");
    // It stays down: a tenth below it.
    budget.on_remb(2_800_000, t0 + ms(2500));
    assert_eq!(budget.wire(), 2_520_000);
    assert_eq!(budget.encoder(), 2_217_600);
    // (Without another report too: the last one counts for ten seconds.)
    let mut alone = Budget::new(8_000_000, true, false, t0);
    alone.on_remb(2_800_000, t0);
    alone.update(t0 + ms(999));
    assert_eq!(alone.wire(), 8_000_000);
    alone.update(t0 + ms(1000));
    assert_eq!(alone.wire(), 2_520_000);
    // The estimate is back at 9.0: about a tenth more each second, not a jump.
    let mut now = t0 + ms(2500);
    let mut before = budget.wire();
    for second in 1..=5 {
        budget.on_remb(9_000_000, now + ms(500));
        now += ms(1000);
        budget.update(now);
        let wire = budget.wire();
        assert!(wire > before && wire as f64 <= before as f64 * 1.12 + 60_000.0, "second {second}: {before} -> {wire}");
        before = wire;
    }
    assert!((3_800_000..4_600_000).contains(&before), "after five seconds: {before}");
    for _ in 0..12 {
        budget.on_remb(9_000_000, now + ms(500));
        now += ms(1000);
        budget.update(now);
    }
    assert_eq!(budget.wire(), 8_000_000, "and it ends at what was announced, not above");
    // An estimate of nothing is not followed below the floor.
    settled(&mut budget, 0, now);
    assert_eq!(budget.wire(), MIN_WIRE);
    assert_eq!(budget.encoder(), 440_000);
}

#[test]
fn golive_rate_forgets_an_estimate_that_is_not_renewed() {
    let t0 = Instant::now();
    let mut budget = Budget::new(8_000_000, true, false, t0);
    let now = settled(&mut budget, 2_000_000, t0);
    assert_eq!((budget.wire(), budget.estimate(now + ms(9_000))), (1_800_000, Some(2_000_000)));
    budget.update(now + ms(9_900));
    assert_eq!(budget.wire(), 1_800_000, "held while the estimate is less than ten seconds old");
    // No further report: the budget rises again, slowly.
    budget.update(now + ms(11_000));
    assert!(budget.wire() > 1_800_000 && budget.wire() < 2_400_000, "{}", budget.wire());
    assert_eq!(budget.estimate(now + ms(11_000)), None);
    // LITECORD_GOLIVE_REMB=0: shown, not followed.
    let mut shown = Budget::new(8_000_000, false, false, t0);
    let now = settled(&mut shown, 2_000_000, t0);
    assert_eq!((shown.wire(), shown.estimate(now)), (8_000_000, Some(2_000_000)));
    assert_eq!(shown.ask(now + ms(10_000)), (7_040_000, Ask::Same));
}

#[test]
fn golive_rate_asked_of_the_encoder_moves_in_steps() {
    // The fork's GPU encoders start a new sequence (a keyframe) with every new bitrate:
    // an estimate that wobbles must not become a keyframe every few seconds.
    let t0 = Instant::now();
    let mut budget = Budget::new(8_000_000, true, false, t0);
    // The first live run with two viewers: estimates between 8.3 and 9.0, a report a
    // second. Nine tenths of 8.4 is 7.56, 5.5 % under 8.0: not a step for the encoder.
    let mut now = t0;
    for estimate in [9_000_000u64, 8_500_000, 8_600_000, 9_000_000, 8_400_000, 8_400_000, 9_000_000] {
        now += ms(1000);
        budget.on_remb(estimate, now);
        assert_eq!(budget.ask(now), (7_040_000, Ask::Same), "estimate {estimate}");
    }
    // 7.5 and staying there: a cut (6.75 on the wire, 15.6 % less), taken when the second is up.
    for _ in 0..10 {
        now += ms(1000);
        budget.on_remb(9_000_000, now);
    }
    budget.on_remb(7_500_000, now);
    assert_eq!(budget.ask(now).1, Ask::Same);
    now = settled(&mut budget, 7_500_000, now);
    assert_eq!(budget.ask(now), (5_940_000, Ask::Cut));
    assert_eq!(budget.ask(now + ms(1)), (5_940_000, Ask::Same));
    // Further down: 2.7 % less is not a step; 3.0 is.
    now = settled(&mut budget, 7_300_000, now + ms(100));
    assert_eq!(budget.ask(now).1, Ask::Same, "2.7 % less is not a step");
    now = settled(&mut budget, 3_000_000, now + ms(100));
    assert_eq!(budget.ask(now), (2_376_000, Ask::Cut));
    let cut_at = now;
    // The estimate is back at 9.0 at once. Nothing is asked for eight seconds, then what
    // the budget has reached by then, then everything.
    let mut asked = Vec::new();
    for step in 1..=1000u64 {
        now = cut_at + ms(step * 20);
        if step % 10 == 0 {
            budget.on_remb(9_000_000, now);
        }
        budget.update(now);
        if let (rate, Ask::Raised) = budget.ask(now) {
            asked.push((now.duration_since(cut_at).as_millis() as u64, rate));
        }
    }
    assert_eq!(asked.len(), 2, "two steps back to the top: {asked:?}");
    assert!((8_000..8_100).contains(&asked[0].0) && (4_500_000..6_200_000).contains(&asked[0].1), "{asked:?}");
    assert!((16_000..16_200).contains(&asked[1].0) && asked[1].1 == 7_040_000, "{asked:?}");
    assert_eq!(budget.ask(now), (7_040_000, Ask::Same));
    // A rise of less than 15 % that is not the way to the top is not worth a keyframe.
    now = settled(&mut budget, 4_000_000, now);
    assert_eq!(budget.ask(now), (3_168_000, Ask::Cut));
    for second in 1..=30u64 {
        budget.on_remb(4_400_000, now + ms(second * 1000));
        assert_eq!(budget.ask(now + ms(second * 1000)).1, Ask::Same);
    }
}

#[test]
fn golive_rate_keeps_the_wire_under_the_budget() {
    // The first live run: 8.0 asked of the encoder, 8.3 to 9.4 Mbit/s on the wire. Here
    // packets add the usual 7.5 %: 7.04 asked, 7.57 on the wire, nothing to correct.
    let t0 = Instant::now();
    let mut budget = Budget::new(8_000_000, true, false, t0);
    let (now, changes) = run(&mut budget, t0, 30.0, |_| 1.075);
    assert_eq!((budget.asked(), budget.encoder_share(), changes), (7_040_000, 88, 0));
    // An encoder that gives a tenth more than it is asked for (and keyframes on top): the
    // wire is over the budget, and the encoder is asked for less until it is not.
    let (now, changes) = run(&mut budget, now, 40.0, |_| 1.075 * 1.12);
    let wire = budget.asked() as f64 * 1.075 * 1.12;
    assert!(wire <= 8_000_000.0, "the wire carries {wire:.0} of 8 000 000 allowed");
    assert!(budget.encoder_share() <= 83 && budget.encoder_share() >= 80, "the encoder is asked for {} %", budget.encoder_share());
    assert_eq!(changes, 1, "in one step: one keyframe");
    // It behaves again: the share comes back a point every ten seconds, and the rate
    // asked follows once that is everything again.
    let share = budget.encoder_share();
    let (now, _) = run(&mut budget, now, 50.0, |_| 1.075);
    assert!(budget.encoder_share() >= share + 4 && budget.encoder_share() <= share + 5, "{} % after {share} %", budget.encoder_share());
    let (_, _) = run(&mut budget, now, 120.0, |_| 1.075);
    assert_eq!((budget.encoder_share(), budget.asked()), (88, 7_040_000));
    // An easy picture (the encoder gives a third of what it may) changes nothing.
    let mut easy = Budget::new(8_000_000, true, false, t0);
    let (_, changes) = run(&mut easy, t0, 20.0, |_| 0.3);
    assert_eq!((easy.wire(), easy.encoder_share(), changes), (8_000_000, 88, 0));
    // A cut is not held against the encoder while it settles on the new rate.
    let mut cut = Budget::new(8_000_000, true, false, t0);
    let (now, _) = run(&mut cut, t0, 5.0, |_| 1.075);
    let now = settled(&mut cut, 4_000_000, now);
    assert_eq!(cut.ask(now), (3_168_000, Ask::Cut));
    // (For a second the encoder still makes what it was asked before.)
    let mut now = now;
    for _ in 0..50 {
        now += ms(20);
        cut.on_sent(7_600_000 / 8 / 50);
        cut.on_remb(4_000_000, now);
    }
    let mut now2 = now;
    for second in 0..8 {
        cut.on_remb(4_000_000, now2);
        (now2, _) = run(&mut cut, now2, 1.0, |_| 1.075);
        assert_eq!(cut.encoder_share(), 88, "second {second} after the cut");
    }
    assert_eq!((cut.wire(), cut.asked()), (3_600_000, 3_168_000));
    // Nobody watches: what was measured is forgotten.
    cut.idle(now2 + ms(100));
    assert_eq!(cut.encoder_share(), 88);
}

#[test]
fn golive_pacer_spreads_a_keyframe_and_lets_small_frames_through() {
    let t0 = Instant::now();
    let packet = 1240usize;
    // Sends `packets` full packets from `at` on, a millisecond per turn; returns how long
    // the last one took and the most that left in one turn.
    let drain = |pacer: &mut Pacer, at: Instant, packets: usize| -> (u64, usize) {
        let (mut left, mut turn, mut most) = (packets, 0u64, 0usize);
        loop {
            let now = at + ms(turn);
            pacer.refill(now, left * packet, ms(turn));
            let mut sent = 0;
            while left > 0 && pacer.ready() {
                pacer.took(packet);
                left -= 1;
                sent += 1;
            }
            most = most.max(sent);
            if left == 0 {
                return (turn, most);
            }
            turn += 1;
            assert!(turn < 2_000, "the pacer never let the packets go");
        }
    };
    // 8 Mbit/s allowed, 2.5 times that: 20 Mbit/s, about two packets a millisecond.
    let mut pacer = Pacer::new(25, t0);
    pacer.set_budget(8_000_000);
    assert_eq!(pacer.rate_bps(), 20_000_000);
    // A delta frame of 12 packets: ten at once, the rest within a millisecond or two.
    let (took, most) = drain(&mut pacer, t0 + ms(100), 12);
    assert!(took <= 2 && most <= 11, "{took} ms, {most} at once");
    // A keyframe of 110 packets (what the first live run sent in one burst of 48 and 12 a
    // millisecond after): about 50 ms, never more than a dozen at once.
    let (took, most) = drain(&mut pacer, t0 + ms(200), 110);
    assert!((40..=60).contains(&took) && most <= 11, "{took} ms, {most} at once");
    // A low budget: the floor of 3 Mbit/s, and what is queued still leaves within 150 ms.
    pacer.set_budget(600_000);
    assert_eq!(pacer.rate_bps(), 3_000_000);
    let (took, _) = drain(&mut pacer, t0 + ms(1_000), 110);
    assert!((100..=160).contains(&took), "{took} ms");
    // LITECORD_GOLIVE_PACE=burst: the first version, 48 at once and 12 a millisecond.
    let mut burst = Pacer::new(0, t0);
    burst.set_budget(8_000_000);
    let (took, most) = drain(&mut burst, t0 + ms(100), 110);
    assert!(took <= 6 && most == 48, "{took} ms, {most} at once");
    // An empty bucket says how long to wait.
    let mut pacer = Pacer::new(25, t0);
    pacer.refill(t0 + ms(50), 0, Duration::ZERO);
    assert_eq!(pacer.wait(), Duration::ZERO);
    while pacer.ready() {
        pacer.took(packet);
    }
    assert!(pacer.wait() > Duration::ZERO && pacer.wait() < ms(2));
}

#[test]
fn golive_keyframes_are_granted_once_a_second_for_everybody() {
    let t0 = Instant::now();
    let mut gate = KeyframeGate::new(ms(1000));
    // The first one at once.
    assert!(gate.request(t0, false));
    assert_eq!(gate.due(), None);
    // The first live run: up to 18 PLI in five seconds, each answered with a keyframe.
    for n in 1..=18 {
        assert!(!gate.request(t0 + ms(n * 50), false), "request {n} within the second");
    }
    assert_eq!(gate.due(), Some(t0 + ms(1000)), "kept: whoever asked still needs one");
    assert!(!gate.take_due(t0 + ms(999)));
    assert!(gate.take_due(t0 + ms(1000)));
    assert!(!gate.take_due(t0 + ms(1001)), "once");
    assert_eq!(gate.due(), None);
    // The keyframe goes out a little later; the second counts from there.
    gate.sent(t0 + ms(1020));
    assert!(!gate.request(t0 + ms(2000), false));
    assert_eq!(gate.due(), Some(t0 + ms(2020)));
    assert!(gate.take_due(t0 + ms(2020)));
    // A stream that sends nothing until its keyframe comes may ask sooner, but not on
    // every picture.
    assert!(!gate.request(t0 + ms(2100), true));
    assert_eq!(gate.due(), Some(t0 + ms(2320)));
    assert!(gate.request(t0 + ms(2320), true));
    assert!(!gate.request(t0 + ms(2400), true) && !gate.request(t0 + ms(2400), false));
    assert_eq!(gate.due(), Some(t0 + ms(2620)), "the sooner of the two is kept");
    // The share's own periodic keyframe counts too.
    let mut gate = KeyframeGate::new(ms(1000));
    gate.sent(t0);
    assert!(!gate.request(t0 + ms(400), false));
    assert_eq!(gate.last(), Some(t0));
}

/// The steps between the first 901 pictures of the fourth live run, in 90 kHz ticks: a
/// 144 Hz monitor shared at 60 pictures a second, as the capture let them pass (1, 2, 3, 4
/// or 5 refreshes of 6.9 ms apart; 15 s). From the packet log of that run.
pub(super) const RUN4_STEPS: [u32; 900] = [
    517, 1253, 2044, 1106, 1250, 1948, 1229, 1913, 1903, 631, 1877, 1971, 1248, 1338, 1975, 613, 1605, 2146, 1898, 636, 1863, 1907, 558, 1918, 1241,
    1837, 1267, 1913, 688, 1527, 2147, 1800, 1260, 1328, 1179, 1895, 1249, 1193, 2493, 631, 1873, 1253, 1248, 1948, 1233, 1879, 1236, 1825, 1112, 1484,
    1861, 1251, 656, 2462, 1864, 561, 1971, 1150, 1939, 1260, 1894, 1301, 1326, 1721, 1876, 642, 1884, 1249, 1851, 1089, 2196, 1768, 1197, 1193, 1938,
    810, 1685, 1287, 1800, 1310, 1349, 1841, 1264, 1986, 1114, 1877, 1243, 1205, 1879, 1245, 2509, 606, 1311, 1825, 1244, 1848, 1294, 1241, 1838, 1930,
    592, 2527, 660, 1842, 1882, 608, 1874, 1914, 1206, 1244, 1370, 1829, 1833, 1222, 1311, 1189, 1920, 1851, 1280, 1219, 1316, 1820, 1881, 1231, 1275,
    1891, 1236, 1228, 1822, 1291, 1316, 1772, 1893, 1260, 1344, 1167, 1844, 2003, 583, 1837, 1855, 1510, 992, 2564, 654, 1227, 2410, 660, 1908, 1211,
    1258, 1805, 1332, 1825, 1289, 1262, 1836, 1226, 1895, 1853, 654, 2486, 1261, 660, 1867, 626, 2454, 1245, 2492, 641, 1299, 1808, 1293, 1861, 1278,
    1204, 1934, 1215, 1906, 1214, 1284, 1874, 1288, 1176, 1883, 1875, 1283, 1221, 1252, 1876, 1293, 1841, 1245, 1882, 1261, 1241, 1868, 1258, 1246, 1932,
    1204, 1879, 1247, 1250, 1886, 1247, 1875, 1251, 1251, 1890, 1230, 1882, 1881, 625, 1875, 1875, 1884, 622, 1302, 1829, 1878, 1879, 618, 1260, 1873,
    1246, 1883, 1303, 1193, 1876, 1252, 1883, 1247, 1247, 1879, 1258, 1878, 1248, 1246, 1877, 1253, 1881, 1250, 1249, 1881, 1251, 1868, 1255, 1256, 1879,
    1249, 1875, 1257, 1243, 1876, 1258, 1878, 1245, 1253, 1872, 1247, 1880, 1283, 1218, 1886, 1233, 1880, 1249, 1260, 1879, 1249, 1249, 1905, 1252, 1870,
    1257, 1224, 1887, 1242, 1872, 1503, 1000, 1879, 1249, 1887, 641, 1851, 1862, 1257, 1898, 1886, 622, 1859, 1239, 2521, 632, 1242, 2115, 999, 1911,
    1218, 1247, 2106, 1251, 1888, 1241, 1059, 1849, 1255, 2098, 1020, 1264, 2079, 1050, 1871, 1479, 1030, 2064, 1062, 2076, 1246, 1244, 2312, 604, 2100,
    1385, 934, 1856, 1280, 1837, 1260, 1457, 1993, 1265, 1730, 1253, 1045, 2384, 566, 2263, 1373, 1115, 1873, 1251, 1878, 1245, 1044, 2071, 2094, 627,
    1488, 1426, 2502, 571, 1962, 1237, 1306, 1799, 1308, 2490, 573, 1266, 1867, 1261, 1644, 1494, 1361, 2450, 645, 2509, 647, 1260, 1725, 2021, 562,
    1800, 1032, 2857, 561, 1941, 1099, 1398, 1517, 1461, 1869, 1021, 1287, 2070, 1430, 1693, 1240, 1262, 1886, 1424, 2137, 775, 1105, 2011, 1101, 1909,
    1985, 1247, 1117, 1216, 2045, 555, 1798, 2013, 1093, 2158, 1117, 1260, 1774, 1347, 2501, 1270, 581, 2577, 569, 1746, 1516, 1192, 1872, 1252, 1971,
    1254, 1160, 1870, 1069, 2029, 1260, 1235, 1981, 1781, 658, 1955, 957, 2726, 561, 1923, 1024, 1471, 1852, 1274, 1982, 1145, 1255, 1966, 1158, 1867,
    1249, 1237, 1867, 1246, 1880, 1895, 649, 1860, 1040, 2457, 1484, 624, 1870, 1254, 1861, 1262, 1286, 1828, 1286, 1853, 1892, 1222, 1242, 2000, 1242,
    1258, 907, 2137, 1845, 619, 1883, 1251, 1870, 1266, 1857, 1278, 1038, 2067, 1968, 630, 1862, 1174, 1969, 1156, 1865, 1040, 2187, 594, 2210, 1518,
    1247, 1249, 1912, 1245, 1869, 1215, 1011, 2114, 1296, 1828, 1257, 1243, 2513, 626, 1863, 1246, 998, 2140, 1274, 2211, 899, 1237, 1615, 1507, 1878,
    650, 1557, 2172, 997, 2122, 1247, 1271, 1864, 1237, 1886, 1254, 996, 2120, 953, 2181, 1246, 1230, 1892, 1283, 1840, 1228, 1893, 1250, 1281, 2175,
    1564, 968, 1261, 1510, 1894, 1226, 1276, 1836, 1012, 1881, 1242, 1505, 2243, 878, 1882, 1247, 1287, 2468, 566, 2302, 889, 1250, 2471, 624, 1879,
    1240, 1005, 2142, 1297, 1825, 1260, 1240, 1882, 1225, 1903, 998, 1238, 1936, 1179, 2111, 1234, 1886, 1243, 1260, 1890, 1218, 1070, 2066, 1276, 2499,
    1239, 612, 1891, 1240, 1879, 1278, 999, 1866, 1282, 2109, 1268, 1253, 1876, 1217, 1886, 1251, 1259, 1840, 1007, 2130, 1249, 1250, 2497, 622, 1892,
    1248, 995, 2156, 930, 2174, 1243, 1251, 1879, 1271, 2488, 616, 952, 2195, 1246, 1853, 1266, 1227, 1894, 1249, 1592, 924, 1543, 2195, 1861, 630,
    1869, 1240, 1892, 1236, 1880, 1249, 983, 2163, 975, 2497, 1255, 1260, 1441, 1050, 2102, 1229, 1008, 2758, 565, 1729, 1245, 1259, 1859, 1248, 1857,
    1480, 1254, 2500, 561, 1927, 1239, 1037, 2106, 1231, 1675, 1464, 1054, 2065, 1280, 1855, 1231, 1292, 1841, 1262, 2619, 634, 1145, 1851, 1252, 1988,
    1243, 1255, 1561, 1438, 1866, 1258, 1265, 1868, 1041, 2505, 823, 1266, 1983, 1244, 1743, 1267, 1060, 2051, 1880, 737, 1755, 1894, 1366, 1127, 1996,
    1243, 1155, 1857, 1047, 2203, 1114, 1267, 1872, 1380, 1786, 1223, 1041, 2187, 921, 2538, 1416, 759, 1758, 1998, 750, 1746, 1738, 1400, 894, 2219,
    1233, 1259, 1744, 1260, 1874, 1235, 1082, 2068, 1020, 2094, 1252, 1256, 1661, 1244, 2650, 835, 546, 2997, 570, 1954, 1205, 1260, 1778, 1227, 1860,
    1534, 973, 2015, 1113, 1993, 1123, 1375, 1866, 1276, 1844, 1258, 1144, 1874, 1244, 2223, 1030, 1116, 2033, 1233, 2693, 633, 926, 2211, 930, 1989,
    1120, 1594, 1649, 1449, 2506, 620, 1044, 2101, 1047, 2091, 566, 1913, 1873, 1242, 1876, 1251, 907, 2841, 612, 1689, 2059, 1045, 1466, 1249, 1881,
    1247, 1044, 1848, 1298, 2046, 1690, 1432, 564, 1603, 2021, 1246, 1885, 1246, 1102, 2228, 1055, 1433, 1875, 1697, 560, 1954, 1249, 1841, 1122, 2650,
    815, 1239, 1701, 1249, 2474, 629, 1875, 1246, 1256, 1873, 2080, 1222, 567, 1943, 1882, 614, 2521, 597, 1705, 1731, 1400, 1459, 1666, 2077, 614,
    2509, 612, 1882, 893, 2237, 1245, 1874, 566, 1717, 1885, 1261, 1097, 2026, 1247, 2087, 1225, 1266, 1858, 1275, 2480, 637, 1058, 1880, 1720, 2048,
];

/// What a 60 Hz screen that shows each picture at the first refresh at or after its
/// timestamp makes of a stream: pictures never shown (another came before the refresh),
/// and refreshes that show the picture before again.
pub(super) fn on_a_60_hz_screen(stamps: &[u32]) -> (usize, usize) {
    let (mut never, mut repeats, mut last_slot) = (0, 0, None::<u64>);
    for stamp in stamps {
        let slot = (stamp.wrapping_sub(stamps[0]) as u64).div_ceil(1_500);
        match last_slot {
            Some(last) if slot == last => never += 1,
            Some(last) => repeats += (slot - last - 1) as usize,
            None => {}
        }
        if last_slot != Some(slot) {
            last_slot = Some(slot);
        }
    }
    (never, repeats)
}

/// The clock timestamps of a source that steps by `steps` (in ticks), over and over.
fn source(steps: &[f64], pictures: usize) -> Vec<u32> {
    let mut at = 1_000_000.0f64;
    let mut out = vec![at as u32];
    for n in 0..pictures {
        at += steps[n % steps.len()];
        out.push(at as u32);
    }
    out
}

fn on_the_grid(clock: &[u32], fps: u32) -> Vec<u32> {
    let mut even = EvenClock::new(fps);
    clock.iter().map(|c| even.stamp(*c)).collect()
}

fn steps_of(stamps: &[u32]) -> Vec<i64> {
    stamps.windows(2).map(|w| w[1].wrapping_sub(w[0]) as i32 as i64).collect()
}

#[test]
fn golive_even_timestamps_for_the_pictures_of_the_fourth_live_run() {
    let mut clock = vec![3_000_000_000u32];
    for step in RUN4_STEPS {
        clock.push(clock.last().unwrap().wrapping_add(step));
    }
    // As it was sent: one picture in ten only 7 ms after the one before, one in seventeen
    // 25 ms or more after. On a 60 Hz screen 28 of these 901 pictures are never shown, and
    // 29 refreshes repeat a picture: part of the stutter the viewers saw.
    let sent = steps_of(&clock);
    assert_eq!((sent.iter().filter(|s| **s < 900).count(), sent.iter().filter(|s| **s > 2_200).count()), (86, 54));
    assert_eq!(on_a_60_hz_screen(&clock), (28, 29));
    // The source keeps the share's pace (60 a second on average), which shows after its
    // first 24 pictures: from there on every picture is one frame interval after the one
    // before, and none is lost on that screen.
    let mut even = EvenClock::new(60);
    let grid: Vec<u32> = clock.iter().map(|c| even.stamp(*c)).collect();
    assert!(even.engaged());
    assert_eq!(grid[..24], clock[..24], "until the pace is known: the clock's timestamps");
    let steps = steps_of(&grid[30..]);
    let (least, most) = (*steps.iter().min().unwrap(), *steps.iter().max().unwrap());
    assert!(least >= 1_440 && most <= 1_580, "steps from {least} to {most} ticks");
    // (One refresh in these fifteen seconds repeats a picture: the grid follows the source,
    // which is not the screen's own clock.)
    let (never_shown, repeated) = on_a_60_hz_screen(&grid[30..]);
    assert!(never_shown == 0 && repeated <= 2, "{never_shown} never shown, {repeated} repeated");
    // The grid stays near the clock (lip sync, and how long a picture is held to leave
    // evenly): within 14 ms here, 27 ms over the whole run.
    let off: Vec<i32> = grid.iter().zip(&clock).map(|(g, c)| g.wrapping_sub(*c) as i32).collect();
    let (behind, ahead) = (*off.iter().min().unwrap(), *off.iter().max().unwrap());
    assert!(behind >= -1_300 && ahead <= 1_300, "the grid is {behind} to {ahead} ticks from the clock");
}

#[test]
fn golive_even_timestamps_leave_other_sources_alone() {
    let refresh = 90_000.0 / 144.0;
    let untouched = |name: &str, clock: &[u32], fps: u32| {
        let mut even = EvenClock::new(fps);
        for c in clock {
            assert_eq!(even.stamp(*c), *c, "{name}: the clock's timestamp");
            assert!(!even.engaged() || name.contains("60"), "{name}: not on the grid");
        }
    };
    // The capture once its own rhythm is fixed: one picture per three refreshes of a
    // 144 Hz monitor is a steady 48 a second, of a 165 Hz monitor 55. Even already: a
    // grid of 60 a second over them would show three pictures 16.7 ms apart and the
    // fourth 33 ms after.
    untouched("48 a second", &source(&[3.0 * refresh], 1_000), 60);
    untouched("55 a second", &source(&[3.0 * 90_000.0 / 165.0], 1_000), 60);
    // A game at half the share's pace, and a still screen repeated every 100 ms.
    untouched("30 a second", &source(&[3_000.0], 600), 60);
    untouched("a still screen", &source(&[9_000.0], 100), 60);
    // A steady 60: on the grid, which is the clock's own.
    untouched("60 a second", &source(&[1_500.0], 1_000), 60);
    // A game hovering around 50 (2 or 3 refreshes at a time): not the share's pace.
    let hovering: Vec<f64> = (0..997u32).map(|n| if n.wrapping_mul(2_654_435_761) >> 30 == 0 { 2.0 * refresh } else { 3.0 * refresh }).collect();
    untouched("about 50 a second", &source(&hovering, 3_000), 60);

    // What does keep the share's pace is put on the grid: 2, 2, 3, 2 and 3 refreshes
    // apart (60 a second from 144 Hz, as evenly as that goes), and a capture a little
    // slow (59.94 a second).
    for steps in [&[2.0 * refresh, 2.0 * refresh, 3.0 * refresh, 2.0 * refresh, 3.0 * refresh][..], &[90_000.0 / 59.94][..]] {
        let clock = source(steps, 3_000);
        let grid = on_the_grid(&clock, 60);
        assert!(steps_of(&grid[30..]).iter().all(|s| (1_480..=1_520).contains(s)));
        assert!(grid.iter().zip(&clock).all(|(g, c)| (g.wrapping_sub(*c) as i32).abs() <= 500), "within 6 ms of the clock");
    }
    // A share at 30 pictures a second has places of 3000 ticks.
    let grid = on_the_grid(&source(&[2_500.0, 3_500.0], 600), 30);
    assert!(steps_of(&grid[30..]).iter().all(|s| (2_950..=3_050).contains(s)));
    // A picture that never came (one in a hundred): the ones after it are a little
    // further apart for a moment, nothing jumps.
    let mut gappy = vec![1_500.0; 99];
    gappy.push(3_000.0);
    let grid = on_the_grid(&source(&gappy, 1_000), 60);
    assert!(steps_of(&grid[30..]).iter().all(|s| (1_500..=1_560).contains(s)));
}

#[test]
fn golive_even_timestamps_come_and_go_without_a_jump() {
    // The pictures of the fourth live run (on the grid, which is up to 14 ms off the
    // clock by then), and then the source changes its pace to a steady 48 a second: the
    // grid is left, and what it was off the clock by fades instead of jumping.
    let mut clock = vec![1_000u32];
    for step in RUN4_STEPS {
        clock.push(clock.last().unwrap().wrapping_add(step));
    }
    for _ in 0..300 {
        clock.push(clock.last().unwrap().wrapping_add(1_875));
    }
    let mut even = EvenClock::new(60);
    let mut engaged = Vec::new();
    let grid: Vec<u32> = clock.iter().map(|c| {
        let stamp = even.stamp(*c);
        engaged.push(even.engaged());
        stamp
    }).collect();
    assert!(engaged[900] && !engaged[1_200]);
    let left_at = (901..1_201).find(|n| !engaged[*n]).unwrap();
    assert!(left_at <= 960, "the grid is left within a second of the new pace (picture {left_at})");
    let steps = steps_of(&grid[880..]);
    assert!(steps.iter().all(|s| (1_300..=2_400).contains(s)), "steps from {} to {} around the change", steps.iter().min().unwrap(), steps.iter().max().unwrap());
    assert_eq!(grid[1_100..], clock[1_100..], "and the timestamps are the clock's again");

    // A long pause: back on the clock at once. Two pictures at the same instant: never the
    // same timestamp twice.
    let mut even = EvenClock::new(60);
    assert_eq!(even.stamp(5_000), 5_000, "the first picture is on the clock");
    assert_eq!(even.stamp(6_500), 6_500);
    assert_eq!(even.stamp(906_500), 906_500);
    assert_eq!(even.stamp(906_500), 906_501);
    assert_eq!(even.stamp(908_000), 908_000);
    // Across the wrap of the 32-bit timestamp, on the grid.
    let mut even = EvenClock::new(60);
    let near = u32::MAX - 40 * 1_500;
    let wrapping: Vec<u32> = (0..80u32).map(|n| near.wrapping_add(n * 1_500 + if n % 2 == 0 { 0 } else { 300 })).collect();
    let grid: Vec<u32> = wrapping.iter().map(|c| even.stamp(*c)).collect();
    assert!(even.engaged());
    assert!(steps_of(&grid[30..]).iter().all(|s| (1_480..=1_520).contains(s)));
    even.reset();
    assert!(!even.engaged());
    assert_eq!(even.stamp(77), 77);
}

#[test]
fn golive_audio_reports_do_not_follow_the_batches_of_the_capture() {
    // The loopback delivers its chunks in batches: packets leave 0 to 20 ms after their
    // last sample. A sender report must not name the timestamp of whichever came last.
    let t0 = Instant::now();
    let mut clock = AudioClock::new();
    assert!(!clock.is_set());
    let mut timestamp = 48_000u32;
    let mut worst = 0i64;
    for n in 0..1_500u64 {
        // (Every third packet is on time; the others waited 10 or 20 ms for their batch.)
        let late = [0u64, 10, 20][(n % 3) as usize];
        let sent = t0 + ms(20 * n + 20 + late);
        timestamp = timestamp.wrapping_add(960);
        clock.on_packet(timestamp, sent);
        if n > 3 {
            // What the report says the capture is at, against the truth: the sample
            // captured at `sent` (the first one was at t0, with timestamp 48 000 + 960).
            let truth = 48_960.0 + sent.duration_since(t0).as_secs_f64() * 48_000.0;
            let off = clock.timestamp_at(sent) as i64 - truth as i64;
            worst = worst.max(off.abs());
        }
    }
    assert!(clock.is_set());
    assert!(worst <= 96, "the report is up to {worst} samples (of 48 000 a second) off; the last packet's own timestamp would be up to 1920 off");
    // A capture clock that runs slow against the machine's is followed within the window.
    let mut clock = AudioClock::new();
    let mut timestamp = 0u32;
    let mut sent = t0;
    for _ in 0..6_000u64 {
        // (20 ms of samples take 20.01 ms: 500 ppm slow.)
        sent += Duration::from_micros(20_010);
        timestamp = timestamp.wrapping_add(960);
        clock.on_packet(timestamp, sent);
    }
    let off = clock.timestamp_at(sent) as i64 - (timestamp as i64 + 960);
    assert!(off.abs() <= 300, "{off} samples off after two minutes of drift");
    clock.reset();
    assert!(!clock.is_set());
    assert_eq!(clock.timestamp_at(sent), 0);
}
