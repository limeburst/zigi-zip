use rand::seq::SliceRandom;

/// Short, memorable English words; curated to avoid ambiguous/offensive/look-alike terms.
const WORDS: &[&str] = &[
    "amber", "anchor", "apple", "arrow", "artist", "atlas", "autumn", "bacon",
    "badger", "banjo", "basil", "beacon", "beaver", "birch", "bison", "bloom",
    "boulder", "branch", "breeze", "bridge", "bright", "bronze", "bubble", "bucket",
    "bundle", "butter", "cactus", "camel", "candle", "canvas", "canyon", "carbon",
    "cargo", "carrot", "castle", "cedar", "cello", "cherry", "chess", "cider",
    "cinder", "circle", "clever", "clover", "cobalt", "cocoa", "comet", "copper",
    "coral", "cosmos", "cotton", "crane", "crater", "crimson", "crystal", "cyan",
    "dahlia", "daisy", "dapper", "dazzle", "desert", "diamond", "dolphin", "dragon",
    "dream", "drift", "dusk", "eagle", "ember", "emerald", "empire", "engine",
    "falcon", "feather", "fennel", "fiber", "finch", "flame", "fjord", "forest",
    "fossil", "fountain", "fox", "galaxy", "garden", "gecko", "ginger", "glacier",
    "granite", "grape", "gravel", "hazel", "heron", "hickory", "honey", "horizon",
    "indigo", "island", "ivory", "jade", "jasper", "journey", "juniper", "kayak",
    "kettle", "lagoon", "lantern", "laurel", "lavender", "lemon", "lichen", "lilac",
    "linen", "lotus", "lumber", "magnet", "magpie", "mango", "maple", "marble",
    "marigold", "marlin", "meadow", "mellow", "meteor", "midnight", "mint", "mirror",
    "misty", "moss", "moth", "mountain", "nectar", "nimble", "noble", "nomad",
    "north", "nova", "oasis", "oak", "ocean", "onyx", "opal", "orbit",
    "orchid", "otter", "panda", "paper", "pebble", "pepper", "petal", "phoenix",
    "pickle", "pigeon", "pilot", "pine", "pixel", "planet", "plum", "poppy",
    "prairie", "puffin", "pumpkin", "quartz", "quince", "rabbit", "radar", "raven",
    "ribbon", "ridge", "river", "robin", "rocket", "rose", "rune", "saffron",
    "sage", "salmon", "sapphire", "scarlet", "seal", "silver", "sparrow", "spruce",
    "squirrel", "starlit", "stellar", "stone", "storm", "summit", "sunrise", "swallow",
    "tango", "teal", "thistle", "thunder", "tiger", "timber", "topaz", "torch",
    "tulip", "turtle", "twilight", "umber", "valley", "velvet", "violet", "walnut",
    "whisker", "willow", "winter", "wombat", "yarrow", "zephyr",
];

pub fn two_word() -> String {
    let mut rng = rand::thread_rng();
    let a = WORDS.choose(&mut rng).unwrap();
    let b = WORDS.choose(&mut rng).unwrap();
    format!("{} {}", a, b)
}
