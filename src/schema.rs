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

pub const CREATE_FILE_ASSOCIATIONS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS file_associations (
    dataset_id TEXT,
    source_file_path TEXT,
    target_file_path TEXT,
    association_type TEXT,
    PRIMARY KEY (dataset_id, source_file_path, target_file_path, association_type),
    FOREIGN KEY (dataset_id, source_file_path) REFERENCES scans(dataset_id, file_path),
    FOREIGN KEY (dataset_id, target_file_path) REFERENCES scans(dataset_id, file_path)
);
";
