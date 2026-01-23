use rusqlite::{Connection, Result, params};

const DB_PATH: &str = "tiles.db";

/// Initialize the database and create tables if they don't exist
pub fn init_db() -> Result<Connection> {
    let conn = Connection::open(DB_PATH)?;
    
    // Create table for visited tiles with first visit timestamp and activity info
    conn.execute(
        "CREATE TABLE IF NOT EXISTS tiles (
            x INTEGER NOT NULL,
            y INTEGER NOT NULL,
            z INTEGER NOT NULL,
            first_visited_at INTEGER NOT NULL,
            activity_id TEXT,
            activity_title TEXT,
            gpx_filename TEXT,
            PRIMARY KEY (x, y, z)
        )",
        [],
    )?;
    
    // Create table to track processed GPX files
    conn.execute(
        "CREATE TABLE IF NOT EXISTS processed_files (
            filename TEXT PRIMARY KEY,
            processed_at INTEGER NOT NULL
        )",
        [],
    )?;
    
    // Create table to track imported Strava activities
    conn.execute(
        "CREATE TABLE IF NOT EXISTS imported_activities (
            activity_id INTEGER PRIMARY KEY,
            activity_name TEXT,
            imported_at INTEGER NOT NULL,
            distance_km REAL DEFAULT 0.0
        )",
        [],
    )?;
    
    // Migration: Add distance_km column if it doesn't exist (for existing databases)
    let _ = conn.execute(
        "ALTER TABLE imported_activities ADD COLUMN distance_km REAL DEFAULT 0.0",
        [],
    );
    
    Ok(conn)
}

/// Check if a GPX file has already been processed
pub fn is_file_processed(conn: &Connection, filename: &str) -> Result<bool> {
    let count: i32 = conn.query_row(
        "SELECT COUNT(*) FROM processed_files WHERE filename = ?1",
        params![filename],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Mark a GPX file as processed
pub fn mark_file_processed(conn: &Connection, filename: &str) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    
    conn.execute(
        "INSERT OR IGNORE INTO processed_files (filename, processed_at) VALUES (?1, ?2)",
        params![filename, now],
    )?;
    Ok(())
}

/// Tile data for batch insert
pub struct TileData {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub visited_at: i64,
    pub activity_id: String,
    pub activity_title: String,
    pub gpx_filename: String,
}

/// Insert multiple tiles in a transaction
pub fn insert_tiles_batch(conn: &mut Connection, tiles: &[TileData]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO tiles (x, y, z, first_visited_at, activity_id, activity_title, gpx_filename) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(x, y, z) DO UPDATE SET 
                first_visited_at = MIN(first_visited_at, excluded.first_visited_at),
                activity_id = CASE WHEN excluded.first_visited_at < first_visited_at THEN excluded.activity_id ELSE activity_id END,
                activity_title = CASE WHEN excluded.first_visited_at < first_visited_at THEN excluded.activity_title ELSE activity_title END,
                gpx_filename = CASE WHEN excluded.first_visited_at < first_visited_at THEN excluded.gpx_filename ELSE gpx_filename END"
        )?;
        
        for tile in tiles {
            stmt.execute(params![tile.x, tile.y, tile.z, tile.visited_at, tile.activity_id, tile.activity_title, tile.gpx_filename])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Get all visited tiles from the database
pub fn get_all_tiles(conn: &Connection) -> Result<Vec<TileRecord>> {
    let mut stmt = conn.prepare("SELECT x, y, z, first_visited_at, activity_id, activity_title, gpx_filename FROM tiles")?;
    let tiles = stmt.query_map([], |row| {
        Ok(TileRecord {
            x: row.get(0)?,
            y: row.get(1)?,
            z: row.get(2)?,
            first_visited_at: row.get(3)?,
            activity_id: row.get(4)?,
            activity_title: row.get(5)?,
            gpx_filename: row.get(6)?,
        })
    })?;
    
    tiles.collect()
}

/// Get tile count
pub fn get_tile_count(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM tiles", [], |row| row.get(0))?;
    Ok(count as usize)
}

#[derive(Debug)]
pub struct TileRecord {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub first_visited_at: i64,
    pub activity_id: Option<String>,
    pub activity_title: Option<String>,
    pub gpx_filename: Option<String>,
}

/// Check if an activity has already been imported from Strava
pub fn is_activity_imported(conn: &Connection, activity_id: i64) -> Result<bool> {
    let count: i32 = conn.query_row(
        "SELECT COUNT(*) FROM imported_activities WHERE activity_id = ?1",
        params![activity_id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Mark an activity as imported from Strava
pub fn mark_activity_imported(conn: &Connection, activity_id: i64, activity_name: Option<&str>, distance_km: f64) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    
    conn.execute(
        "INSERT OR IGNORE INTO imported_activities (activity_id, activity_name, imported_at, distance_km) VALUES (?1, ?2, ?3, ?4)",
        params![activity_id, activity_name, now, distance_km],
    )?;
    Ok(())
}

/// Get all imported activity IDs
pub fn get_imported_activity_ids(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT activity_id FROM imported_activities")?;
    let ids = stmt.query_map([], |row| row.get(0))?;
    ids.collect()
}

/// Get total distance of all imported activities in km
pub fn get_total_distance(conn: &Connection) -> Result<f64> {
    let total: f64 = conn.query_row(
        "SELECT COALESCE(SUM(distance_km), 0.0) FROM imported_activities",
        [],
        |row| row.get(0),
    )?;
    Ok(total)
}
