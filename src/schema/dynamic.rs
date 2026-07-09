use duckdb::{Connection, Result};
use serde_json::Value;
use std::collections::HashMap;

const SCHEMA_JSON: &str = include_str!("../data/schema.json");

#[derive(Clone, Debug)]
pub struct Schema {
    schema: Value,
    table_definitions: HashMap<String, String>,
    table_columns: HashMap<String, Vec<(String, String, String)>>, // table -> [(col_name, col_type, json_key)]
    primary_keys: HashMap<String, Vec<String>>,                    // table -> [pk_col_names]
}

impl Schema {
    pub fn load(schema_path: Option<&str>) -> Self {
        let schema: Value = match schema_path {
            Some(path) => {
                let content = std::fs::read_to_string(path)
                    .unwrap_or_else(|e| panic!("Failed to read schema file {}: {}", path, e));
                serde_json::from_str(&content)
                    .unwrap_or_else(|e| panic!("Failed to parse schema file {}: {}", path, e))
            }
            None => {
                serde_json::from_str(SCHEMA_JSON).expect("Failed to parse embedded schema.json")
            }
        };

        let mut instance = Schema {
            schema,
            table_definitions: HashMap::new(),
            table_columns: HashMap::new(),
            primary_keys: HashMap::new(),
        };
        instance.generate_definitions();
        instance
    }

    fn generate_definitions(&mut self) {
        self.generate_scans_tables();

        self.generate_participants_table();

        self.generate_table_def("sessions", "Sessions", vec![
            ("dataset_id", "TEXT", "dataset_id"),
            ("session_id", "TEXT", "session_id"),
            ("participant_id", "TEXT", "participant_id"),
        ], vec![
            "other_data JSON",
            "PRIMARY KEY (dataset_id, participant_id, session_id)",
            "FOREIGN KEY (dataset_id, participant_id) REFERENCES participants(dataset_id, participant_id)",
        ]);

        // Events table
        self.generate_table_def(
            "events",
            "Events",
            vec![
                ("dataset_id", "TEXT", "dataset_id"),
                ("file_path", "TEXT", "filename"),
            ],
            vec!["onset FLOAT", "duration FLOAT", "other_data JSON"],
        );

        // Dataset Description
        self.generate_dataset_description_def();
    }

    /// Extract all metadata field definitions from schema.json
    fn extract_metadata_fields(&self) -> HashMap<String, (String, bool)> {
        let mut fields = HashMap::new();

        if let Some(metadata) = self.schema["objects"]["metadata"].as_object() {
            for (field_name, field_def) in metadata {
                let sql_type = self.json_type_to_sql(field_def);
                // For now, assume all fields are nullable (we'll refine this later based on REQUIRED/RECOMMENDED)
                fields.insert(field_name.clone(), (sql_type, true));
            }
        }

        fields
    }

    /// Map JSON schema type to SQL type
    fn json_type_to_sql(&self, field_def: &Value) -> String {
        // Handle anyOf (union types) - take the first non-null type
        if let Some(any_of) = field_def.get("anyOf").and_then(|v| v.as_array()) {
            for type_def in any_of {
                if let Some(t) = type_def.get("type").and_then(|v| v.as_str()) {
                    if t != "null" {
                        return self.map_simple_type(t, type_def);
                    }
                }
            }
        }

        // Handle direct type
        if let Some(type_str) = field_def.get("type").and_then(|v| v.as_str()) {
            return self.map_simple_type(type_str, field_def);
        }

        // Handle BIDS "Format" field (e.g. for age)
        // Some fields like age have "Format": "number" inside definition
        if let Some(format) = field_def.get("Format").and_then(|v| v.as_str()) {
            match format {
                "number" | "float" => return "DOUBLE".to_string(),
                "integer" => return "BIGINT".to_string(),
                "boolean" => return "BOOLEAN".to_string(),
                _ => {} // Fallback to TEXT
            }
        }

        // Check inside "definition" object if present (common in BIDS schema)
        if let Some(def) = field_def.get("definition") {
            if let Some(format) = def.get("Format").and_then(|v| v.as_str()) {
                match format {
                    "number" | "float" => return "DOUBLE".to_string(),
                    "integer" => return "BIGINT".to_string(),
                    "boolean" => return "BOOLEAN".to_string(),
                    _ => {}
                }
            }
        }

        // Default to TEXT for unknown types
        "TEXT".to_string()
    }

    /// Map a simple JSON type to SQL type
    fn map_simple_type(&self, json_type: &str, field_def: &Value) -> String {
        match json_type {
            "string" => {
                // Check for special formats
                if let Some(format) = field_def.get("format").and_then(|v| v.as_str()) {
                    match format {
                        "datetime" => "TIMESTAMP",
                        "date" => "DATE",
                        _ => "TEXT",
                    }
                } else {
                    "TEXT"
                }
            }
            "number" => "DOUBLE",
            "integer" => "BIGINT",
            "boolean" => "BOOLEAN",
            "array" => "TEXT", // Store as JSON text
            "object" => "JSON",
            _ => "TEXT",
        }
        .to_string()
    }

    /// Generate scans and sidecars tables
    fn generate_scans_tables(&mut self) {
        // 1. Generate 'scans' table (minimal, based on rules)
        let mut scans_columns = Vec::new();
        let mut scans_fields = Vec::new();

        // Base columns for scans
        scans_columns.push("dataset_id TEXT".to_string());
        scans_fields.push((
            "dataset_id".to_string(),
            "TEXT".to_string(),
            "dataset_id".to_string(),
        ));

        scans_columns.push("file_path TEXT".to_string());
        scans_fields.push((
            "file_path".to_string(),
            "TEXT".to_string(),
            "file_path".to_string(),
        ));

        // Extract fields from rules.tabular_data.modality_agnostic.Scans
        if let Some(objs) = self
            .schema
            .get("rules")
            .and_then(|r| r.get("tabular_data"))
            .and_then(|t| t.get("modality_agnostic"))
            .and_then(|m| m.get("Scans"))
            .and_then(|p| p.get("columns"))
            .and_then(|c| c.as_object())
        {
            let mut keys: Vec<&String> = objs.keys().collect();
            keys.sort();

            for key in keys {
                if key == "filename" {
                    continue;
                } // Already added as file_path

                // Look up definition in objects.columns for type info
                let field_def = self
                    .schema
                    .get("objects")
                    .and_then(|o| o.get("columns"))
                    .and_then(|c| c.get(key));

                let sql_type = if let Some(def) = field_def {
                    self.json_type_to_sql(def)
                } else {
                    "TEXT".to_string()
                };

                let col_name = to_snake_case(key);
                scans_columns.push(format!("{} {}", col_name, sql_type));
                scans_fields.push((col_name, sql_type, key.clone()));
            }
        }

        // Add other_data to scans for any extra TSV columns
        scans_columns.push("other_data JSON".to_string());
        scans_fields.push((
            "other_data".to_string(),
            "JSON".to_string(),
            "other_data".to_string(),
        ));

        // Constraints for scans
        scans_columns.push("PRIMARY KEY (dataset_id, file_path)".to_string());

        let scans_sql = format!(
            "CREATE TABLE IF NOT EXISTS scans (\n    {}\n);",
            scans_columns.join(",\n    ")
        );
        self.table_definitions
            .insert("scans".to_string(), scans_sql);
        self.table_columns.insert("scans".to_string(), scans_fields);
        self.primary_keys.insert(
            "scans".to_string(),
            vec!["dataset_id".to_string(), "file_path".to_string()],
        );

        // 2. Generate 'sidecars' table (wide, all metadata)
        let mut sidecar_columns = Vec::new();
        let mut sidecar_fields = Vec::new();
        let mut seen_lowercase_names = std::collections::HashSet::new();

        // Base columns for sidecars
        sidecar_columns.push("dataset_id TEXT".to_string());
        sidecar_fields.push((
            "dataset_id".to_string(),
            "TEXT".to_string(),
            "dataset_id".to_string(),
        ));
        seen_lowercase_names.insert("dataset_id".to_string());

        sidecar_columns.push("file_path TEXT".to_string());
        sidecar_fields.push((
            "file_path".to_string(),
            "TEXT".to_string(),
            "file_path".to_string(),
        ));
        seen_lowercase_names.insert("file_path".to_string());

        // Extract ALL metadata fields from objects.metadata
        let metadata_fields = self.extract_metadata_fields();
        let mut sorted_fields: Vec<_> = metadata_fields.into_iter().collect();
        sorted_fields.sort_by(|a, b| a.0.cmp(&b.0));

        for (field_name, (sql_type, _nullable)) in sorted_fields {
            let col_name = to_snake_case(&field_name);

            if seen_lowercase_names.contains(&col_name) {
                continue;
            }

            sidecar_columns.push(format!("{} {}", col_name, sql_type));
            sidecar_fields.push((col_name.clone(), sql_type, field_name));
            seen_lowercase_names.insert(col_name);
        }

        // Add other_data for custom fields in sidecars
        sidecar_columns.push("other_data JSON".to_string());
        sidecar_fields.push((
            "other_data".to_string(),
            "JSON".to_string(),
            "other_data".to_string(),
        ));

        // Constraints for sidecars
        sidecar_columns.push("PRIMARY KEY (dataset_id, file_path)".to_string());
        sidecar_columns.push(
            "FOREIGN KEY (dataset_id, file_path) REFERENCES scans(dataset_id, file_path)"
                .to_string(),
        );

        let sidecar_sql = format!(
            "CREATE TABLE IF NOT EXISTS sidecars (\n    {}\n);",
            sidecar_columns.join(",\n    ")
        );
        self.table_definitions
            .insert("sidecars".to_string(), sidecar_sql);
        self.table_columns
            .insert("sidecars".to_string(), sidecar_fields);
        self.primary_keys.insert(
            "sidecars".to_string(),
            vec!["dataset_id".to_string(), "file_path".to_string()],
        );
    }

    fn generate_table_def(
        &mut self,
        table_name: &str,
        schema_key: &str,
        base_fields: Vec<(&str, &str, &str)>, // (col_name, col_type, json_key)
        extra_columns: Vec<&str>,
    ) {
        let mut columns = Vec::new();
        let mut fields = Vec::new();

        // Add base fields
        for (col_name, col_type, json_key) in base_fields {
            columns.push(format!("{} {}", col_name, col_type));
            fields.push((
                col_name.to_string(),
                col_type.to_string(),
                json_key.to_string(),
            ));
        }

        // Add fields from schema
        if let Some(rules) =
            self.schema["rules"]["modality_agnostic"][schema_key]["columns"].as_object()
        {
            for (key, _) in rules {
                // Skip fields that are already in base_fields or explicitly handled
                if !fields.iter().any(|(_, _, k)| k == key) {
                    let col_name = to_snake_case(key);
                    columns.push(format!("{} TEXT", col_name));
                    fields.push((col_name, "TEXT".to_string(), key.clone()));
                }
            }
        }

        // Add extra columns (like foreign keys, primary keys, or manually added columns)
        for col in extra_columns {
            columns.push(col.to_string());

            // Heuristic to extract PKs
            if col.starts_with("PRIMARY KEY") {
                // Extract columns inside parentheses: PRIMARY KEY (col1, col2)
                if let Some(start) = col.find('(') {
                    if let Some(end) = col.find(')') {
                        let pk_cols: Vec<String> = col[start + 1..end]
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .collect();
                        self.primary_keys.insert(table_name.to_string(), pk_cols);
                    }
                }
            }

            // If the extra column is a data column (not a constraint), add it to fields if it's not there
            // This is a bit heuristic, assuming constraints start with PRIMARY or FOREIGN
            if !col.starts_with("PRIMARY") && !col.starts_with("FOREIGN") {
                let parts: Vec<&str> = col.split_whitespace().collect();
                if parts.len() >= 2 {
                    let col_name = parts[0];
                    let col_type = parts[1];
                    // Check if it's already in fields (e.g. other_data)
                    if !fields.iter().any(|(n, _, _)| n == col_name) {
                        fields.push((
                            col_name.to_string(),
                            col_type.to_string(),
                            col_name.to_string(),
                        ));
                    }
                }
            }
        }

        let create_sql = format!(
            "CREATE TABLE IF NOT EXISTS {} (\n    {}\n);",
            table_name,
            columns.join(",\n    ")
        );

        self.table_definitions
            .insert(table_name.to_string(), create_sql);
        self.table_columns.insert(table_name.to_string(), fields);
    }

    fn generate_dataset_description_def(&mut self) {
        let table_name = "dataset_description";
        let mut columns = vec![
            "dataset_id TEXT PRIMARY KEY".to_string(),
            "root_uri TEXT".to_string(),
        ];
        let mut fields = vec![
            (
                "dataset_id".to_string(),
                "TEXT".to_string(),
                "dataset_id".to_string(),
            ),
            (
                "root_uri".to_string(),
                "TEXT".to_string(),
                "root_uri".to_string(),
            ),
        ];
        let mut seen_lowercase_names = std::collections::HashSet::new();
        seen_lowercase_names.insert("dataset_id".to_lowercase());
        seen_lowercase_names.insert("root_uri".to_lowercase());

        // List of known dataset_description.json fields from BIDS spec
        // These are the fields that typically appear in dataset_description.json
        let dataset_description_field_names = vec![
            "Name",
            "BIDSVersion",
            "HEDVersion",
            "DatasetType",
            "License",
            "Authors",
            "Acknowledgements",
            "HowToAcknowledge",
            "Funding",
            "EthicsApprovals",
            "ReferencesAndLinks",
            "DatasetDOI",
            "GeneratedBy",
            "SourceDatasets",
            "DatasetLinks",
        ];

        // Extract these fields from the metadata object
        if let Some(metadata) = self.schema["objects"]["metadata"].as_object() {
            for field_name in dataset_description_field_names {
                if let Some(field_def) = metadata.get(field_name) {
                    let sql_type = self.json_type_to_sql(field_def);
                    let col_name = to_snake_case(field_name);
                    let lowercase_name = col_name.to_lowercase();

                    if !seen_lowercase_names.contains(&lowercase_name) {
                        columns.push(format!("{} {}", col_name, sql_type));
                        fields.push((col_name.clone(), sql_type, field_name.to_string()));
                        seen_lowercase_names.insert(lowercase_name);
                    }
                }
            }
        }

        // Add other_data for custom fields
        columns.push("other_data JSON".to_string());
        fields.push((
            "other_data".to_string(),
            "JSON".to_string(),
            "other_data".to_string(),
        ));

        let create_sql = format!(
            "CREATE TABLE IF NOT EXISTS {} (\n    {}\n);",
            table_name,
            columns.join(",\n    ")
        );
        self.table_definitions
            .insert(table_name.to_string(), create_sql);
        self.table_columns.insert(table_name.to_string(), fields);
        self.primary_keys
            .insert(table_name.to_string(), vec!["dataset_id".to_string()]);
    }

    fn generate_participants_table(&mut self) {
        let mut columns = Vec::new();
        let mut fields = Vec::new();
        let mut seen_lowercase_names = std::collections::HashSet::new();

        // Base columns
        columns.push("dataset_id TEXT".to_string());
        fields.push((
            "dataset_id".to_string(),
            "TEXT".to_string(),
            "dataset_id".to_string(),
        ));
        seen_lowercase_names.insert("dataset_id".to_string());

        columns.push("participant_id TEXT".to_string());
        fields.push((
            "participant_id".to_string(),
            "TEXT".to_string(),
            "participant_id".to_string(),
        ));
        seen_lowercase_names.insert("participant_id".to_string());

        // Extract fields from rules.tabular_data.modality_agnostic.Participants
        if let Some(objs) = self
            .schema
            .get("rules")
            .and_then(|r| r.get("tabular_data"))
            .and_then(|t| t.get("modality_agnostic"))
            .and_then(|m| m.get("Participants"))
            .and_then(|p| p.get("columns"))
            .and_then(|c| c.as_object())
        {
            // Sort keys for deterministic output
            let mut keys: Vec<&String> = objs.keys().collect();
            keys.sort();

            for key in keys {
                // Skip participant_id as we already added it
                if key == "participant_id" {
                    continue;
                }

                // Look up definition in objects.columns to get type info
                // The rules object just says "optional"/"required", we need the definition for type
                let field_def = self
                    .schema
                    .get("objects")
                    .and_then(|o| o.get("columns"))
                    .and_then(|c| c.get(key));

                let sql_type = if let Some(def) = field_def {
                    self.json_type_to_sql(def)
                } else {
                    "TEXT".to_string()
                };

                let col_name = to_snake_case(key);
                if seen_lowercase_names.contains(&col_name) {
                    continue;
                }

                columns.push(format!("{} {}", col_name, sql_type));
                fields.push((col_name.clone(), sql_type, key.clone()));
                seen_lowercase_names.insert(col_name);
            }
        }

        // Add other_data for custom fields
        columns.push("other_data JSON".to_string());
        fields.push((
            "other_data".to_string(),
            "JSON".to_string(),
            "other_data".to_string(),
        ));

        // Add constraints
        columns.push("PRIMARY KEY (dataset_id, participant_id)".to_string());

        let create_sql = format!(
            "CREATE TABLE IF NOT EXISTS participants (\n    {}\n);",
            columns.join(",\n    ")
        );

        self.table_definitions
            .insert("participants".to_string(), create_sql);
        self.table_columns
            .insert("participants".to_string(), fields);
        self.primary_keys.insert(
            "participants".to_string(),
            vec!["dataset_id".to_string(), "participant_id".to_string()],
        );
    }

    pub fn create_tables_sql(&self) -> Vec<String> {
        let mut sqls = Vec::new();
        // Order matters for foreign keys
        let order = vec![
            "participants",
            "sessions",
            "scans",
            "sidecars",
            "events",
            "dataset_description",
        ];
        for table in order {
            if let Some(sql) = self.table_definitions.get(table) {
                sqls.push(sql.clone());
            }
        }
        // Add other tables that might not be in the map yet if we add more later
        // For now, this covers the main ones.
        // We also need to handle tables that are static like 'files' and 'diffusion' if they are not generated from schema
        // But the prompt implies we should generate what we can.
        // The original code had CREATE_FILES_TABLE and CREATE_DIFFUSION_TABLE as static strings in schema_generated or schema.rs?
        // Let's check schema.rs later. For now, we return what we generated.
        sqls
    }

    #[allow(dead_code)]
    pub fn get_create_sql(&self, table_name: &str) -> Option<&String> {
        self.table_definitions.get(table_name)
    }

    pub fn insert(&self, conn: &Connection, table_name: &str, data: &Value) -> Result<()> {
        let fields = self.table_columns.get(table_name).ok_or_else(|| {
            duckdb::Error::ToSqlConversionFailure(Box::new(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Table {} not found in schema", table_name),
            )))
        })?;

        // Build set of all json_keys that have dedicated columns
        let schema_keys: std::collections::HashSet<&str> = fields
            .iter()
            .map(|(_, _, json_key)| json_key.as_str())
            .collect();

        let col_names: Vec<&str> = fields.iter().map(|(n, _, _)| n.as_str()).collect();

        // Generate numbered placeholders $1, $2, ...
        let placeholders: Vec<String> = (1..=col_names.len()).map(|i| format!("${}", i)).collect();

        // Construct SQL
        let mut sql = format!(
            "INSERT INTO {} ({}) SELECT {}",
            table_name,
            col_names.join(", "),
            placeholders.join(", ")
        );

        // Add WHERE NOT EXISTS clause if we have primary keys
        if let Some(pks) = self.primary_keys.get(table_name) {
            if !pks.is_empty() {
                let where_clauses: Vec<String> = pks
                    .iter()
                    .map(|pk| {
                        // Find index of this PK in fields to get correct $N
                        let idx = fields
                            .iter()
                            .position(|(name, _, _)| name == pk)
                            .expect("Primary key column not found in fields");
                        format!("{} = ${}", pk, idx + 1)
                    })
                    .collect();

                sql.push_str(&format!(
                    " WHERE NOT EXISTS (SELECT 1 FROM {} WHERE {})",
                    table_name,
                    where_clauses.join(" AND ")
                ));
            }
        }

        let mut params = Vec::new();
        // We need to keep the values alive until the end of the function
        let mut string_values: Vec<String> = Vec::new();

        for (col_name, col_type, json_key) in fields {
            // Special handling for other_data column
            if col_name == "other_data" {
                if let Some(obj) = data.as_object() {
                    // Filter: only include keys NOT in schema_keys
                    let mut custom_data = serde_json::Map::new();
                    for (key, value) in obj {
                        if !schema_keys.contains(key.as_str()) {
                            custom_data.insert(key.clone(), value.clone());
                        }
                    }

                    if !custom_data.is_empty() {
                        let json_str = serde_json::to_string(&custom_data).unwrap();
                        string_values.push(json_str);
                        params.push(Some(duckdb::types::Value::Text(
                            string_values.last().unwrap().clone(),
                        )));
                    } else {
                        params.push(None);
                    }
                } else {
                    params.push(None);
                }
                continue;
            }

            let val = if let Some(obj) = data.as_object() {
                obj.get(json_key).or_else(|| obj.get(col_name))
            } else {
                None
            };

            if col_type == "JSON" {
                let s = val.map(|v| v.to_string());
                if let Some(s_val) = s {
                    string_values.push(s_val);
                    params.push(Some(duckdb::types::Value::Text(
                        string_values.last().unwrap().clone(),
                    )));
                } else {
                    params.push(None);
                }
            } else {
                match val {
                    Some(Value::String(s)) => {
                        // Handle "n/a" for numeric columns (BIDS convention for missing values)
                        if (col_type == "DOUBLE"
                            || col_type == "BIGINT"
                            || col_type == "FLOAT"
                            || col_type == "REAL")
                            && s == "n/a"
                        {
                            params.push(None);
                        } else {
                            params.push(Some(duckdb::types::Value::Text(s.clone())));
                        }
                    }
                    Some(Value::Number(n)) => {
                        if n.is_i64() {
                            params.push(Some(duckdb::types::Value::BigInt(n.as_i64().unwrap())));
                        } else if n.is_f64() {
                            params.push(Some(duckdb::types::Value::Double(n.as_f64().unwrap())));
                        } else {
                            params.push(Some(duckdb::types::Value::Text(n.to_string())));
                        }
                    }
                    Some(Value::Bool(b)) => {
                        params.push(Some(duckdb::types::Value::Boolean(*b)));
                    }
                    Some(Value::Null) | None => {
                        params.push(None);
                    }
                    Some(v) => {
                        // Fallback for other types to string if expected type is TEXT
                        // For arrays, serialize to JSON string
                        if v.is_array() || v.is_object() {
                            let json_str = v.to_string();
                            string_values.push(json_str);
                            params.push(Some(duckdb::types::Value::Text(
                                string_values.last().unwrap().clone(),
                            )));
                        } else {
                            params.push(Some(duckdb::types::Value::Text(v.to_string())));
                        }
                    }
                }
            }
        }

        // Convert Option<Value> to &dyn ToSql
        let params_refs: Vec<&dyn duckdb::ToSql> =
            params.iter().map(|p| p as &dyn duckdb::ToSql).collect();

        conn.execute(&sql, params_refs.as_slice())?;
        Ok(())
    }
}

fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    let chars: Vec<char> = s.chars().collect();

    for (i, c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            // Add underscore before uppercase if:
            // 1. Not at start (i > 0)
            // 2. Previous char was lowercase, OR
            // 3. Next char exists and is lowercase (end of acronym like "XMLParser" -> "xml_parser")
            if i > 0 {
                let prev_is_lower = chars.get(i - 1).is_some_and(|ch| ch.is_lowercase());
                let next_is_lower = chars.get(i + 1).is_some_and(|ch| ch.is_lowercase());

                if prev_is_lower || next_is_lower {
                    result.push('_');
                }
            }
            result.push(c.to_lowercase().next().unwrap());
        } else {
            result.push(*c);
        }
    }

    // Normalize multiple underscores
    let mut normalized = String::new();
    let mut prev_was_underscore = false;
    for c in result.chars() {
        if c == '_' {
            if !prev_was_underscore {
                normalized.push(c);
            }
            prev_was_underscore = true;
        } else {
            normalized.push(c);
            prev_was_underscore = false;
        }
    }
    normalized
}
