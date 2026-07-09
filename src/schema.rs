pub mod dynamic;
pub use dynamic::Schema;

pub const CREATE_DIFFUSION_TABLE: &str = "
CREATE TABLE IF NOT EXISTS diffusion (
    dataset_id TEXT,
    file_path TEXT,
    bval DOUBLE[],
    bvec_x DOUBLE[],
    bvec_y DOUBLE[],
    bvec_z DOUBLE[],
    PRIMARY KEY (dataset_id, file_path)
);
";

// file_associations is best-effort, import-time derived metadata (e.g. an
// fmap's IntendedFor, or a coordsystem referencing an anatomical). Its source is
// often a sidecar/JSON that is not itself a `scans` row, so we deliberately do
// NOT enforce foreign keys here — doing so would drop otherwise-valid
// associations during import. Targets are resolved to full dataset-relative
// paths so they still join to `scans` when present.
pub const CREATE_FILE_ASSOCIATIONS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS file_associations (
    dataset_id TEXT,
    source_file_path TEXT,
    target_file_path TEXT,
    association_type TEXT,
    PRIMARY KEY (dataset_id, source_file_path, target_file_path, association_type)
);
";
