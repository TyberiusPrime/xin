struct Hash {
    value: [u8;20], // nix has 160 bits of store hash... seems reasonably long.
    kind: HashKind,
}

enum HashKind {
    Blake3
}

pub struct InputHash([u8;20]);
pub struct OutputHash([u8;20]);


