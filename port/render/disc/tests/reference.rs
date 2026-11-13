//! The USA disc itself, when DHD_ISO names redump 30393's image (read only; no game data in the
//! tree): its files give USA_FILES_SHA256, and src/usa_files.bin holds its files' tags. With
//! DHD_DISC_BLESS=1 it writes usa_files.bin. Without DHD_ISO it checks nothing.
//!
//! DHD_DISC_COPY names a scratch file (not on C:) for the copies test: the image copied, one byte
//! flipped in sector 0 (the system area: the same hash), then one inside a file (another hash, and
//! that file named). The copy is deleted at the end.

use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

fn usa() -> Option<String> {
    std::env::var("DHD_ISO")
        .ok()
        .filter(|p| Path::new(p).is_file())
}

#[test]
fn the_usa_disc_gives_its_value() {
    let Some(iso) = usa() else {
        eprintln!("DHD_ISO isn't set: the USA disc isn't checked");
        return;
    };
    let mut f = fs::File::open(&iso).unwrap();
    let mut entries = dhd_disc::entries(&mut f).unwrap();
    let files = dhd_disc::files_sha256(Path::new(&iso), |_, _| true).unwrap();
    if std::env::var_os("DHD_DISC_BLESS").is_some() {
        // each file's record tag, sorted: the same records hash() makes
        let mut tags = Vec::new();
        entries.sort_by_key(|e| e.sector);
        for e in &entries {
            f.seek(SeekFrom::Start(e.sector as u64 * dhd_disc::SECTOR))
                .unwrap();
            let mut data = vec![0u8; e.size as usize];
            std::io::Read::read_exact(&mut f, &mut data).unwrap();
            let contents = ring::digest::digest(&ring::digest::SHA256, &data);
            let record = dhd_disc::record(&e.path, e.size, contents.as_ref());
            tags.push(ring::digest::digest(&ring::digest::SHA256, &record).as_ref()[..4].to_vec());
        }
        tags.sort();
        let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/usa_files.bin");
        fs::write(out, tags.concat()).unwrap();
        println!("{} {}", files.hex(), files.count);
        return;
    }
    assert_eq!(files.hex(), dhd_disc::USA_FILES_SHA256);
    assert_eq!(files.count, dhd_disc::USA_FILE_COUNT);
    assert_eq!(files.first_other, None);
    assert!(files.is_usa() && files.note().is_empty());
}

#[test]
fn a_copy_differs_only_where_a_file_does() {
    let (Some(iso), Ok(copy)) = (usa(), std::env::var("DHD_DISC_COPY")) else {
        eprintln!("DHD_ISO or DHD_DISC_COPY isn't set: no copies made");
        return;
    };
    let copy = Path::new(&copy);
    assert!(
        !copy
            .to_string_lossy()
            .to_ascii_lowercase()
            .starts_with("c:"),
        "not on C:"
    );
    fs::copy(&iso, copy).unwrap();
    let flip = |at: u64| {
        let mut f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(copy)
            .unwrap();
        let mut b = [0u8];
        f.seek(SeekFrom::Start(at)).unwrap();
        std::io::Read::read_exact(&mut f, &mut b).unwrap();
        f.seek(SeekFrom::Start(at)).unwrap();
        f.write_all(&[b[0] ^ 0x01]).unwrap();
    };
    flip(5); // sector 0
    let system_area = dhd_disc::files_sha256(copy, |_, _| true).unwrap();
    flip(5);
    // a byte in the middle of the largest file
    let mut entries = dhd_disc::entries(&mut fs::File::open(copy).unwrap()).unwrap();
    entries.sort_by_key(|e| e.size);
    let big = entries.last().unwrap().clone();
    flip(big.sector as u64 * dhd_disc::SECTOR + big.size as u64 / 2);
    let in_a_file = dhd_disc::files_sha256(copy, |_, _| true).unwrap();
    fs::remove_file(copy).unwrap();
    println!(
        "sector 0 flipped: {}; {} flipped: {} ({})",
        system_area.hex(),
        big.path,
        in_a_file.hex(),
        in_a_file.note()
    );
    assert_eq!(system_area.hex(), dhd_disc::USA_FILES_SHA256);
    assert_ne!(in_a_file.hex(), dhd_disc::USA_FILES_SHA256);
    assert_eq!(in_a_file.first_other.as_deref(), Some(big.path.as_str()));
}
