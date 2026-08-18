//! The shipped default taxonomy.
//!
//! # Provenance
//!
//! Authored from public format knowledge for this repository. The suffix
//! tables, the category set, the keys, and the palette are **not** transcribed
//! from any other project; QDirStat is GPL-2.0 and docs/04 fixes the
//! rule that we reproduce *behaviour*, never tables. Where a suffix is
//! genuinely ambiguous the choice is written down in a comment next to it,
//! because the interesting part of a taxonomy is the tie-breaks.
//!
//! # Order is a wire contract
//!
//! Declaration order **is** the `CategoryId` assignment, and the id is what
//! crosses IPC — docs/05-UI.md is explicit that Rust sends indices and the
//! frontend resolves colours. Ids 0..=18 therefore reproduce
//! docs/04-CLASSIFICATION.md's "Initial taxonomy" table exactly, read family by
//! family, and `src/lib/categories.ts` derives the same order from the same
//! table. **Insert new categories at the end**; reordering these silently
//! recolours every stored snapshot. `Categorizer::config_hash` is what makes
//! such a change detectable after the fact.
//!
//! Keys are kebab-case for the same reason: they are the stable persistence
//! spelling shared with the frontend's `CategoryKey` union.
//!
//! # Palette
//!
//! Grouped by meaning, not by hue chart: reclaimable bulk (junk, caches, build
//! output, images of disks and machines) reads warm; media reads cool; text,
//! code and documents read green. Colours are settings metadata only.

use crate::schema::{CategoryConfig, CategorySpec, ComponentRule, GlobSpec, Rgb, SCHEMA_VERSION, UNCATEGORIZED_KEY};
use crate::tags::ContextTag::{
    AppleMetadata, BuildOutput, Cache, ContainerStorage, DependencyTree, MediaLibrary, Package, Trash,
};

/// Default number of dot-separated parts to try. `tar.gz` needs two;
/// `tar.gz.gpg` needs three. Three is the point where real archive chains stop.
pub const DEFAULT_MAX_SUFFIX_PARTS: u8 = 3;

/// The shipped, immutable default configuration.
///
/// A user overlay is applied on top of this and re-validated; only a fully
/// valid candidate is compiled and swapped in
/// (docs/04-CLASSIFICATION.md#configuration-lifecycle).
#[must_use]
pub fn default_config() -> CategoryConfig {
    CategoryConfig {
        schema_version: SCHEMA_VERSION,
        max_suffix_parts: DEFAULT_MAX_SUFFIX_PARTS,
        categories: default_categories(),
        components: default_components(),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one table, read top to bottom; splitting it hides the ordering that IS the contract"
)]
fn default_categories() -> Vec<CategorySpec> {
    vec![
        // ================= docs/04 "Initial taxonomy": System =================
        // Index 0 is mandatory and carries no rules: it is the fall-through.
        CategorySpec::new(UNCATEGORIZED_KEY, "Uncategorized", Rgb::new(0x8a, 0x8f, 0x98)),
        // Assigned by `Kind`, never by name.
        CategorySpec::new("symlink", "Symlinks", Rgb::new(0x9a, 0xa0, 0xff)),
        // Assigned by the execute bit when nothing else matched; `.exe` is the
        // one name rule, for Windows binaries sitting in a Downloads folder.
        CategorySpec::new("executable", "Executables", Rgb::new(0xb0, 0x6b, 0xd6)).with_suffixes(&["exe"]),
        // docs/04 calls this "Apple Metadata" and the key follows the doc; the
        // label follows what a user actually wants to see next to a size.
        // Globs, not suffixes: these are whole names, and `._*` is a byte-exact
        // prefix (an AppleDouble twin of any file).
        CategorySpec::new("apple-metadata", "Apple Junk", Rgb::new(0xc2, 0x4e, 0x3a))
            .directories_too()
            .implying(&[AppleMetadata])
            .with_globs(vec![
                GlobSpec::exact_file(".DS_Store"),
                GlobSpec::exact_any("._*"),
                GlobSpec::exact_file(".localized"),
                GlobSpec::exact_file(".apdisk"),
                GlobSpec::exact_file(".com.apple.timemachine.donotpresent"),
                // Finder's custom-icon file is literally "Icon\r".
                GlobSpec::exact_file("Icon\r"),
                GlobSpec::directories(".Spotlight-V100"),
                GlobSpec::directories(".fseventsd"),
                GlobSpec::directories(".DocumentRevisions-V100"),
                GlobSpec::directories(".TemporaryItems"),
                GlobSpec::directories(".Trashes"),
                GlobSpec::directories(".AppleDouble"),
                GlobSpec::directories(".AppleDB"),
                GlobSpec::directories(".AppleDesktop"),
            ]),
        // ================= docs/04 "Initial taxonomy": Archives ===============
        CategorySpec::new("compressed-archive", "Compressed Archives", Rgb::new(0xd0, 0x8a, 0x2e))
            .with_suffixes(&[
                "7z", "aar", "apk", "arj", "asar", "cab", "cbr", "cbz", "crx", "deb", "ear", "egg", "gem", "ipa",
                "jar", "lha", "lzh", "nupkg", "rar", "rpm", "sit", "sitx", "tar.bz2", "tar.gz", "tar.lz4", "tar.lzma",
                "tar.xz", "tar.z", "tar.zst", "tbz", "tbz2", "tgz", "tlz", "txz", "tzst", "vsix", "war", "whl", "xar",
                "xpi", "zip", "zipx",
            ])
            // Historic `compress(1)` chain, uppercase by convention. This is the
            // exact-before-folded path: `x.tar.Z` finds the byte-exact entry at
            // the two-part suffix before anything folds.
            .with_exact_suffixes(&["tar.Z"]),
        CategorySpec::new(
            "uncompressed-archive",
            "Uncompressed Archives",
            Rgb::new(0xc7, 0x9a, 0x4b),
        )
        .with_suffixes(&["cpio", "mar", "pax", "shar", "tar"]),
        CategorySpec::new("compressed-stream", "Compressed Streams", Rgb::new(0xb8, 0x86, 0x3f))
            .with_suffixes(&[
                "br", "bz2", "gz", "lz", "lz4", "lzma", "lzo", "rz", "sz", "xz", "zst", "zz",
            ])
            .with_exact_suffixes(&["Z"]),
        // Directory-eligible for `.sparsebundle` and `.mpkg`, which are bundles.
        CategorySpec::new("disk-image", "Disk Images", Rgb::new(0xe0, 0xa3, 0x3e))
            .directories_too()
            .with_suffixes(&[
                "cdr",
                "dmg",
                "img",
                "iso",
                "mpkg",
                "msi",
                "pkg",
                "smi",
                "sparsebundle",
                "sparseimage",
                "toast",
            ]),
        // ================= docs/04 "Initial taxonomy": Media ==================
        CategorySpec::new("image", "Images", Rgb::new(0x3e, 0x9a, 0xd1)).with_suffixes(&[
            "apng", "avif", "bpg", "gif", "heic", "heics", "heif", "hif", "ico", "icns", "j2k", "jfif", "jp2", "jpe",
            "jpeg", "jpf", "jpg", "jpm", "jpx", "jxl", "png", "qoi", "svg", "svgz", "webp",
        ]),
        // No bare `raw`: it is claimed by too many unrelated tools, and leaving
        // it unclaimed is what lets `Docker.raw` reach the glob stage.
        CategorySpec::new("raw-photo", "RAW Photos", Rgb::new(0x2f, 0x7f, 0xb5)).with_suffixes(&[
            "3fr", "arw", "cr2", "cr3", "crw", "dcr", "dng", "erf", "fff", "iiq", "kdc", "mef", "mos", "mrw", "nef",
            "nrw", "orf", "pef", "raf", "rw2", "rwl", "sr2", "srf", "srw", "x3f",
        ]),
        CategorySpec::new("uncompressed-image", "Uncompressed Images", Rgb::new(0x56, 0xb3, 0xe0)).with_suffixes(&[
            "bmp", "cin", "dds", "dib", "dpx", "exr", "hdr", "pbm", "pcx", "pgm", "pnm", "ppm", "psb", "psd", "ras",
            "sgi", "tga", "tif", "tiff", "xcf", "xpm",
        ]),
        // `ts` is NOT here: on a developer's Mac it is TypeScript far more often
        // than MPEG transport stream, and `mts`/`m2ts` cover the camera case.
        CategorySpec::new("video", "Videos", Rgb::new(0x4a, 0x6f, 0xd1)).with_suffixes(&[
            "3g2", "3gp", "asf", "avi", "braw", "divx", "dv", "f4v", "flv", "m2p", "m2ts", "m2v", "m4v", "mkv", "mov",
            "mp4", "mpe", "mpeg", "mpg", "mts", "mxf", "ogm", "ogv", "r3d", "rm", "rmvb", "vob", "webm", "wmv",
        ]),
        CategorySpec::new("audio", "Audio", Rgb::new(0x3f, 0xb3, 0xa5)).with_suffixes(&[
            "aac", "ac3", "aax", "aif", "aifc", "aiff", "alac", "amr", "ape", "au", "caf", "dff", "dsf", "dts", "flac",
            "m4a", "m4b", "m4p", "m4r", "mid", "midi", "mka", "mp2", "mp3", "mpc", "oga", "ogg", "opus", "ra", "snd",
            "voc", "wav", "wave", "wma", "wv",
        ]),
        // ============ docs/04 "Initial taxonomy": Documents and code ==========
        // `key` is Keynote here. On macOS that is overwhelmingly what it is; a
        // PEM private key is `.pem` far more often than `.key`.
        CategorySpec::new("document", "Documents", Rgb::new(0x4e, 0x9e, 0x6b)).with_suffixes(&[
            "adoc", "azw", "azw3", "bib", "chm", "csv", "djvu", "doc", "docx", "dot", "dotx", "epub", "key",
            "markdown", "md", "mobi", "numbers", "odp", "ods", "odt", "ott", "oxps", "pages", "pdf", "pps", "ppsx",
            "ppt", "pptx", "rst", "rtf", "tex", "text", "tsv", "txt", "wpd", "xls", "xlsb", "xlsm", "xlsx", "xps",
        ]),
        CategorySpec::new("source", "Source", Rgb::new(0x6b, 0xbf, 0x59))
            .with_suffixes(&[
                "asm",
                "astro",
                "bash",
                "bat",
                "c",
                "cc",
                "cfg",
                "cjs",
                "clj",
                "cljc",
                "cljs",
                "cmd",
                "conf",
                "cpp",
                "cr",
                "cs",
                "css",
                "cxx",
                "dart",
                "edn",
                "el",
                "erb",
                "erl",
                "ex",
                "exs",
                "fish",
                "fs",
                "fsx",
                "go",
                "gql",
                "graphql",
                "groovy",
                "h",
                "hcl",
                "hh",
                "hpp",
                "hrl",
                "hs",
                "htm",
                "html",
                "hxx",
                "ini",
                "java",
                "jl",
                "js",
                "json",
                "json5",
                "jsonc",
                "jsx",
                "kt",
                "kts",
                "less",
                "lhs",
                "lisp",
                "lua",
                "m",
                "mjs",
                "ml",
                "mli",
                "mm",
                "nim",
                "nix",
                "pas",
                "php",
                "pl",
                "plist",
                "pm",
                "proto",
                "ps1",
                "py",
                "pyi",
                "pyw",
                "r",
                "rb",
                "rs",
                "s",
                "sass",
                "scala",
                "scss",
                "sh",
                "sql",
                "storyboard",
                "styl",
                "svelte",
                "swift",
                "tf",
                "tfvars",
                "thrift",
                "toml",
                "ts",
                "tsx",
                "vb",
                "vue",
                "xhtml",
                "xib",
                "xml",
                "yaml",
                "yml",
                "zig",
                "zsh",
            ])
            // Uppercase `.C` is C++ and uppercase `.S` is assembly that still
            // wants the preprocessor. Both are lowercase-different languages,
            // which is exactly why the exact map is tried first.
            .with_exact_suffixes(&["C", "S"])
            .with_globs(vec![
                GlobSpec::any("Makefile"),
                GlobSpec::any("GNUmakefile"),
                GlobSpec::any("Dockerfile"),
                GlobSpec::any("Containerfile"),
                GlobSpec::any("Rakefile"),
                GlobSpec::any("Gemfile"),
                GlobSpec::any("Podfile"),
                GlobSpec::any("Brewfile"),
                GlobSpec::any("Justfile"),
                GlobSpec::any("Vagrantfile"),
                GlobSpec::any("Procfile"),
                GlobSpec::any("Fastfile"),
                GlobSpec::any("Appfile"),
                GlobSpec::exact_file(".gitignore"),
                GlobSpec::exact_file(".gitattributes"),
                GlobSpec::exact_file(".gitmodules"),
                GlobSpec::exact_file(".dockerignore"),
                GlobSpec::exact_file(".editorconfig"),
                GlobSpec::exact_file(".env"),
                GlobSpec::exact_file(".bashrc"),
                GlobSpec::exact_file(".bash_profile"),
                GlobSpec::exact_file(".zshrc"),
                GlobSpec::exact_file(".profile"),
                GlobSpec::exact_file(".vimrc"),
                GlobSpec::exact_file(".npmrc"),
                GlobSpec::exact_file(".prettierrc"),
                GlobSpec::exact_file(".eslintrc"),
            ]),
        CategorySpec::new("object-generated", "Object / Generated", Rgb::new(0x93, 0xa2, 0x6b)).with_suffixes(&[
            "bak",
            "bc",
            "car",
            "class",
            "crdownload",
            "d",
            "download",
            "gcda",
            "gcno",
            "gch",
            "ko",
            "ll",
            "lo",
            "lock",
            "log",
            "map",
            "nib",
            "o",
            "obj",
            "orig",
            "part",
            "partial",
            "pch",
            "pyc",
            "pyd",
            "pyo",
            "rej",
            "storyboardc",
            "su",
            "swo",
            "swp",
            "temp",
            "tmp",
        ]),
        CategorySpec::new("library", "Libraries", Rgb::new(0x6f, 0x8f, 0xbf))
            .with_suffixes(&["a", "dll", "dylib", "lib", "node", "rlib", "rmeta", "so", "tbd", "wasm"]),
        // ============ docs/04 "Initial taxonomy": Large runtime data ==========
        // `qcow2` is claimed here rather than by container images: Docker Desktop
        // and Podman both run a Linux VM, so the file really is a VM disk.
        CategorySpec::new("vm-disk", "Virtual Machines", Rgb::new(0xd2, 0x70, 0x3c))
            .directories_too()
            .implying(&[ContainerStorage])
            .with_suffixes(&[
                "hdd", "pvm", "qcow", "qcow2", "utm", "vbox", "vdi", "vhd", "vhdx", "vmdk", "vmem", "vmsd", "vmsn",
                "vmwarevm", "vswp",
            ]),
        // Docker Desktop's disk is `Docker.raw`; Lima/Colima use `diffdisk`.
        // `.raw` is deliberately unclaimed above so the glob fallback is
        // reachable for it.
        CategorySpec::new("container-disk", "Container Images", Rgb::new(0xce, 0x6b, 0x57))
            .directories_too()
            .implying(&[ContainerStorage])
            .with_suffixes(&["oci"])
            .with_globs(vec![
                GlobSpec::exact_any("Docker.raw"),
                GlobSpec::any("diffdisk"),
                GlobSpec::any("basedisk"),
            ]),
        // ===================== macOS additions (id >= 19) =====================
        // Everything below is beyond docs/04's initial table. APPEND ONLY.
        //
        // These are directory-eligible on purpose. A bundle IS a directory, and
        // marking the category directory-only is what stops
        // `Library/Caches/movie.mp4` from being anything but a Video.
        CategorySpec::new("package", "macOS Bundles", Rgb::new(0x8e, 0x6f, 0xd0))
            .directories_too()
            .implying(&[Package])
            .with_suffixes(&[
                "app",
                "appex",
                "bundle",
                "component",
                "docset",
                "framework",
                "kext",
                "lproj",
                "mdimporter",
                "playground",
                "plugin",
                "prefpane",
                "qlgenerator",
                "saver",
                "scptd",
                "service",
                "wdgt",
                "workflow",
                "xcodeproj",
                "xcworkspace",
                "xpc",
            ]),
        CategorySpec::new("media-library", "Photo & Media Libraries", Rgb::new(0x2e, 0x8f, 0xa8))
            .directories_too()
            .implying(&[MediaLibrary])
            .with_suffixes(&[
                "aplibrary",
                "band",
                "fcpbundle",
                "fcpcache",
                "imovielibrary",
                "itl",
                "itlp",
                "logicx",
                "migratedaplibrary",
                "migratedphotolibrary",
                "musiclibrary",
                "photolibrary",
                "photoslibrary",
                "theater",
                "tvlibrary",
            ]),
        CategorySpec::new("build-junk", "Xcode & Build Junk", Rgb::new(0xc8, 0x5a, 0x5a))
            .directories_too()
            .implying(&[BuildOutput])
            .with_suffixes(&[
                "dsym",
                "hmap",
                "swiftdoc",
                "swiftinterface",
                "swiftmodule",
                "swiftsourceinfo",
                "xcactivitylog",
                "xcarchive",
                "xcresult",
                "xctestrun",
                "xcuserdatad",
                "xcuserstate",
            ]),
        // Literal directory names only. Component-aware by construction: a
        // basename glob matches the whole name, so `node_modules.txt` is a
        // Document and `MyCachesBackup` is nothing.
        CategorySpec::new("cache", "Caches", Rgb::new(0xd6, 0x71, 0x4e))
            .directories_too()
            .implying(&[Cache])
            .with_globs(vec![
                GlobSpec::directories("node_modules"),
                GlobSpec::directories("bower_components"),
                GlobSpec::directories("DerivedData"),
                GlobSpec::directories("Caches"),
                GlobSpec::directories(".cache"),
                GlobSpec::directories("__pycache__"),
                GlobSpec::directories(".pytest_cache"),
                GlobSpec::directories(".mypy_cache"),
                GlobSpec::directories(".ruff_cache"),
                GlobSpec::directories(".parcel-cache"),
                GlobSpec::directories(".ccache"),
                GlobSpec::directories(".gradle"),
                GlobSpec::directories(".m2"),
                GlobSpec::directories(".cargo"),
                GlobSpec::directories(".npm"),
                GlobSpec::directories(".yarn"),
                GlobSpec::directories(".pnpm-store"),
                GlobSpec::directories("Pods"),
                GlobSpec::directories("Carthage"),
                GlobSpec::directories(".build"),
                GlobSpec::directories(".next"),
                GlobSpec::directories(".nuxt"),
                GlobSpec::directories(".turbo"),
                GlobSpec::directories(".venv"),
                GlobSpec::directories(".stack-work"),
                GlobSpec::directories(".tox"),
            ]),
        CategorySpec::new("font", "Fonts", Rgb::new(0x8e, 0x7c, 0xc3)).with_suffixes(&[
            "afm", "bdf", "dfont", "fnt", "fon", "otc", "otf", "pcf", "pfb", "pfm", "suit", "ttc", "ttf", "woff",
            "woff2",
        ]),
        CategorySpec::new("database", "Databases", Rgb::new(0x7c, 0xa2, 0x3c)).with_suffixes(&[
            "accdb",
            "db",
            "db-shm",
            "db-wal",
            "db3",
            "dbf",
            "leveldb",
            "mdb",
            "mdbx",
            "realm",
            "s3db",
            "sqlite",
            "sqlite-shm",
            "sqlite-wal",
            "sqlite3",
            "sqlitedb",
            "storedata",
        ]),
    ]
}

/// Directory-component rules for context tagging.
///
/// These are literal component names, matched ASCII-folded against a single
/// path component — never against a whole path and never as a substring
/// (docs/04-CLASSIFICATION.md#context-tagging). They are additive to the tags a
/// component's own *category* implies, so `node_modules` picks up `Cache` from
/// the category and `DependencyTree` from here.
fn default_components() -> Vec<ComponentRule> {
    vec![
        ComponentRule::new("node_modules", &[DependencyTree]),
        ComponentRule::new("bower_components", &[DependencyTree]),
        ComponentRule::new("Pods", &[DependencyTree]),
        ComponentRule::new("Carthage", &[DependencyTree]),
        ComponentRule::new("vendor", &[DependencyTree]),
        ComponentRule::new("venv", &[DependencyTree]),
        ComponentRule::new(".venv", &[DependencyTree]),
        ComponentRule::new(".gradle", &[DependencyTree, Cache]),
        ComponentRule::new(".m2", &[DependencyTree, Cache]),
        ComponentRule::new(".cargo", &[DependencyTree, Cache]),
        ComponentRule::new(".npm", &[DependencyTree, Cache]),
        ComponentRule::new(".yarn", &[DependencyTree, Cache]),
        ComponentRule::new(".pnpm-store", &[DependencyTree, Cache]),
        ComponentRule::new("DerivedData", &[BuildOutput, Cache]),
        ComponentRule::new("build", &[BuildOutput]),
        ComponentRule::new(".build", &[BuildOutput]),
        ComponentRule::new("target", &[BuildOutput]),
        ComponentRule::new(".next", &[BuildOutput, Cache]),
        ComponentRule::new(".nuxt", &[BuildOutput, Cache]),
        ComponentRule::new(".stack-work", &[BuildOutput]),
        ComponentRule::new(".tox", &[BuildOutput, Cache]),
        ComponentRule::new("__pycache__", &[BuildOutput, Cache]),
        ComponentRule::new(".Trash", &[Trash]),
        ComponentRule::new("Trash", &[Trash]),
        ComponentRule::new(".Trashes", &[Trash, AppleMetadata]),
        ComponentRule::new(".MobileBackups", &[AppleMetadata]),
        ComponentRule::new("overlay2", &[ContainerStorage]),
        ComponentRule::new("containerd", &[ContainerStorage]),
        ComponentRule::new("buildkit", &[ContainerStorage]),
        ComponentRule::new("com.docker.docker", &[ContainerStorage]),
        ComponentRule::new("lima", &[ContainerStorage]),
        ComponentRule::new("colima", &[ContainerStorage]),
    ]
}
