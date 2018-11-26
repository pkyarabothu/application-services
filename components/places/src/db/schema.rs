/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

// XXXXXX - This has been cloned from logins/src/schema.rs, on Thom's
// wip-sync-sql-store branch.
// We should work out how to turn this into something that can use a shared
// db.rs.

use crate::db::PlacesDb;
use crate::error::*;
use lazy_static::lazy_static;
use sql_support::ConnExt;

const VERSION: i64 = 2;

const CREATE_TABLE_PLACES_SQL: &str =
    "CREATE TABLE IF NOT EXISTS moz_places (
        id INTEGER PRIMARY KEY,
        url LONGVARCHAR NOT NULL,
        title LONGVARCHAR,
        -- note - desktop has rev_host here - that's now in moz_origin.
        visit_count_local INTEGER NOT NULL DEFAULT 0,
        visit_count_remote INTEGER NOT NULL DEFAULT 0,
        hidden INTEGER DEFAULT 0 NOT NULL,
        typed INTEGER DEFAULT 0 NOT NULL, -- XXX - is 'typed' ok? Note also we want this as a *count*, not a bool.
        frecency INTEGER DEFAULT -1 NOT NULL,
        -- XXX - splitting last visit into local and remote correct?
        last_visit_date_local INTEGER NOT NULL DEFAULT 0,
        last_visit_date_remote INTEGER NOT NULL DEFAULT 0,
        guid TEXT NOT NULL UNIQUE,
        foreign_count INTEGER DEFAULT 0 NOT NULL,
        url_hash INTEGER DEFAULT 0 NOT NULL,
        description TEXT, -- XXXX - title above?
        preview_image_url TEXT,
        -- origin_id would ideally be NOT NULL, but we use a trigger to keep
        -- it up to date, so do perform the initial insert with a null.
        origin_id INTEGER,
        -- a couple of sync-related fields.
        sync_status TINYINT NOT NULL DEFAULT 1, -- 1 is SyncStatus::New
        sync_change_counter INTEGER NOT NULL DEFAULT 0, -- adding visits will increment this

        FOREIGN KEY(origin_id) REFERENCES moz_origins(id) ON DELETE CASCADE
    )";

const CREATE_TABLE_PLACES_TOMBSTONES_SQL: &str =
    "CREATE TABLE IF NOT EXISTS moz_places_tombstones (
        guid TEXT PRIMARY KEY
    ) WITHOUT ROWID";

const CREATE_TABLE_HISTORYVISITS_SQL: &str =
    "CREATE TABLE moz_historyvisits (
        id INTEGER PRIMARY KEY,
        is_local INTEGER NOT NULL, -- XXX - not in desktop - will always be true for visits added locally, always false visits added by sync.
        from_visit INTEGER, -- XXX - self-reference?
        place_id INTEGER NOT NULL,
        visit_date INTEGER NOT NULL,
        visit_type INTEGER NOT NULL,
        -- session INTEGER, -- XXX - what is 'session'? Appears unused.

        FOREIGN KEY(place_id) REFERENCES moz_places(id) ON DELETE CASCADE,
        FOREIGN KEY(from_visit) REFERENCES moz_historyvisits(id)
    )";

const CREATE_TABLE_INPUTHISTORY_SQL: &str = "CREATE TABLE moz_inputhistory (
        place_id INTEGER NOT NULL,
        input LONGVARCHAR NOT NULL,
        use_count INTEGER,

        PRIMARY KEY (place_id, input),
        FOREIGN KEY(place_id) REFERENCES moz_places(id) ON DELETE CASCADE
    )";

// XXX - TODO - moz_annos
// XXX - TODO - moz_anno_attributes
// XXX - TODO - moz_items_annos
// XXX - TODO - moz_bookmarks
// XXX - TODO - moz_bookmarks_deleted

// TODO: This isn't the complete `moz_bookmarks` definition, just enough to
// test autocomplete.
const CREATE_TABLE_BOOKMARKS_SQL: &str = "CREATE TABLE moz_bookmarks (
        id INTEGER PRIMARY KEY,
        fk INTEGER,
        title TEXT,
        lastModified INTEGER NOT NULL DEFAULT 0,

        FOREIGN KEY(fk) REFERENCES moz_places(id) ON DELETE RESTRICT
    )";

// Note: desktop has/had a 'keywords' table, but we intentionally do not.

const CREATE_TABLE_ORIGINS_SQL: &str = "CREATE TABLE moz_origins (
        id INTEGER PRIMARY KEY,
        prefix TEXT NOT NULL,
        host TEXT NOT NULL,
        rev_host TEXT NOT NULL,
        frecency INTEGER NOT NULL, -- XXX - why not default of -1 like in moz_places?
        UNIQUE (prefix, host)
    )";

// Note: Can't be combined with the next trigger, since we can't do an
// `INSERT <blah blah> WHERE <blah>` here.
const CREATE_TRIGGER_MOZPLACES_AFTERDELETE_TOMBSTONES: &str = "
    CREATE TEMP TRIGGER moz_places_afterdelete_trigger_tombstones
    AFTER DELETE ON moz_places
    FOR EACH ROW WHEN OLD.sync_status == 2 -- 2 == SyncStatus::Normal
    BEGIN
        INSERT OR IGNORE INTO moz_places_tombstones (guid) VALUES (OLD.guid);
    END
";

// Note that while we create tombstones manually, we rely on this trigger to
// delete any which might exist when a new record is written to moz_places.
const CREATE_TRIGGER_MOZPLACES_AFTERDELETE_ORIGINS: &str = "
    CREATE TEMP TRIGGER moz_places_afterdelete_trigger_origins
    AFTER DELETE ON moz_places
    FOR EACH ROW
    BEGIN
        INSERT INTO moz_updateoriginsdelete_temp (prefix, host, frecency_delta)
        VALUES (
            get_prefix(OLD.url),
            get_host_and_port(OLD.url),
            -MAX(OLD.frecency, 0)
        )
        ON CONFLICT(prefix, host) DO UPDATE
        SET frecency_delta = frecency_delta - OLD.frecency
        WHERE OLD.frecency > 0;
    END
";

const CREATE_TRIGGER_MOZPLACES_AFTERINSERT_REMOVE_TOMBSTONES: &str = "
    CREATE TEMP TRIGGER moz_places_afterinsert_trigger_tombstone
    AFTER INSERT ON moz_places
    FOR EACH ROW
    BEGIN
        DELETE FROM moz_places_tombstones WHERE guid = NEW.guid;
    END
";

// Triggers which update visit_count and last_visit_date based on historyvisits
// table changes.
const EXCLUDED_VISIT_TYPES: &str = "0, 4, 7, 8, 9"; // stolen from desktop

lazy_static! {
    static ref CREATE_TRIGGER_HISTORYVISITS_AFTERINSERT: String = format!("
        CREATE TEMP TRIGGER moz_historyvisits_afterinsert_trigger
        AFTER INSERT ON moz_historyvisits FOR EACH ROW
        BEGIN
            UPDATE moz_places SET
                visit_count_remote = visit_count_remote + (NEW.visit_type NOT IN ({excluded}) AND NOT(NEW.is_local)),
                visit_count_local =  visit_count_local + (NEW.visit_type NOT IN ({excluded}) AND NEW.is_local),
                last_visit_date_local = MAX(last_visit_date_local,
                                            CASE WHEN NEW.is_local THEN NEW.visit_date ELSE 0 END),
                last_visit_date_remote = MAX(last_visit_date_remote,
                                             CASE WHEN NEW.is_local THEN 0 ELSE NEW.visit_date END)
            WHERE id = NEW.place_id;
        END", excluded = EXCLUDED_VISIT_TYPES);

    static ref CREATE_TRIGGER_HISTORYVISITS_AFTERDELETE: String = format!("
        CREATE TEMP TRIGGER moz_historyvisits_afterdelete_trigger
        AFTER DELETE ON moz_historyvisits FOR EACH ROW
        BEGIN
            UPDATE moz_places SET
                visit_count_local = visit_count_local - (OLD.visit_type NOT IN ({excluded}) AND OLD.is_local),
                visit_count_remote = visit_count_remote - (OLD.visit_type NOT IN ({excluded}) AND NOT(OLD.is_local)),
                last_visit_date_local = IFNULL((SELECT visit_date FROM moz_historyvisits
                                                WHERE place_id = OLD.place_id AND is_local
                                                ORDER BY visit_date DESC LIMIT 1), 0),
                last_visit_date_remote = IFNULL((SELECT visit_date FROM moz_historyvisits
                                                 WHERE place_id = OLD.place_id AND NOT(is_local)
                                                 ORDER BY visit_date DESC LIMIT 1), 0)
            WHERE id = OLD.place_id;
        END", excluded = EXCLUDED_VISIT_TYPES);
}

// XXX - TODO - lots of desktop temp tables - but it's not clear they make sense here yet?

// XXX - TODO - lots of favicon related tables - but it's not clear they make sense here yet?

// This table holds key-value metadata for Places and its consumers. Sync stores
// the sync IDs for the bookmarks and history collections in this table, and the
// last sync time for history.
const CREATE_TABLE_META_SQL: &str = "CREATE TABLE moz_meta (
        key TEXT PRIMARY KEY,
        value NOT NULL
    ) WITHOUT ROWID";

// See https://searchfox.org/mozilla-central/source/toolkit/components/places/nsPlacesIndexes.h
const CREATE_IDX_MOZ_PLACES_URL_HASH: &str = "CREATE INDEX url_hashindex ON moz_places(url_hash)";

// const CREATE_IDX_MOZ_PLACES_REVHOST: &str = "CREATE INDEX hostindex ON moz_places(rev_host)";

const CREATE_IDX_MOZ_PLACES_VISITCOUNT_LOCAL: &str =
    "CREATE INDEX visitcountlocal ON moz_places(visit_count_local)";
const CREATE_IDX_MOZ_PLACES_VISITCOUNT_REMOTE: &str =
    "CREATE INDEX visitcountremote ON moz_places(visit_count_remote)";

const CREATE_IDX_MOZ_PLACES_FRECENCY: &str = "CREATE INDEX frecencyindex ON moz_places(frecency)";

const CREATE_IDX_MOZ_PLACES_LASTVISITDATE_LOCAL: &str =
    "CREATE INDEX lastvisitdatelocalindex ON moz_places(last_visit_date_local)";
const CREATE_IDX_MOZ_PLACES_LASTVISITDATE_REMOTE: &str =
    "CREATE INDEX lastvisitdateremoteindex ON moz_places(last_visit_date_remote)";

const CREATE_IDX_MOZ_PLACES_GUID: &str = "CREATE UNIQUE INDEX guid_uniqueindex ON moz_places(guid)";

const CREATE_IDX_MOZ_PLACES_ORIGIN_ID: &str = "CREATE INDEX originidindex ON moz_places(origin_id)";

const CREATE_IDX_MOZ_HISTORYVISITS_PLACEDATE: &str =
    "CREATE INDEX placedateindex ON moz_historyvisits(place_id, visit_date)";
const CREATE_IDX_MOZ_HISTORYVISITS_FROMVISIT: &str =
    "CREATE INDEX fromindex ON moz_historyvisits(from_visit)";

const CREATE_IDX_MOZ_HISTORYVISITS_VISITDATE: &str =
    "CREATE INDEX dateindex ON moz_historyvisits(visit_date)";

const CREATE_IDX_MOZ_HISTORYVISITS_ISLOCAL: &str =
    "CREATE INDEX islocalindex ON moz_historyvisits(is_local)";

// const CREATE_IDX_MOZ_BOOKMARKS_PLACETYPE: &str = "CREATE INDEX itemindex ON moz_bookmarks(fk, type)";
// const CREATE_IDX_MOZ_BOOKMARKS_PARENTPOSITION: &str = "CREATE INDEX parentindex ON moz_bookmarks(parent, position)";
const CREATE_IDX_MOZ_BOOKMARKS_PLACELASTMODIFIED: &str =
    "CREATE INDEX itemlastmodifiedindex ON moz_bookmarks(fk, lastModified)";
// const CREATE_IDX_MOZ_BOOKMARKS_DATEADDED: &str = "CREATE INDEX dateaddedindex ON moz_bookmarks(dateAdded)";
// const CREATE_IDX_MOZ_BOOKMARKS_GUID: &str = "CREATE UNIQUE INDEX guid_uniqueindex ON moz_bookmarks(guid)";

// This table is used, along with moz_places_afterinsert_trigger, to update
// origins after places removals. During an INSERT into moz_places, origins are
// accumulated in this table, then a DELETE FROM moz_updateoriginsinsert_temp
// will take care of updating the moz_origins table for every new origin. See
// CREATE_PLACES_AFTERINSERT_TRIGGER_ORIGINS below for details.
const CREATE_UPDATEORIGINSINSERT_TEMP: &str = "
    CREATE TEMP TABLE moz_updateoriginsinsert_temp (
        place_id INTEGER PRIMARY KEY,
        prefix TEXT NOT NULL,
        host TEXT NOT NULL,
        rev_host TEXT NOT NULL,
        frecency INTEGER NOT NULL
    )
";

// This table is used in a similar way to moz_updateoriginsinsert_temp, but for
// deletes, and triggered via moz_places_afterdelete_trigger.
//
// When rows are added to this table, moz_places.origin_id may be null.  That's
// why this table uses prefix + host as its primary key, not origin_id.
const CREATE_UPDATEORIGINSDELETE_TEMP: &str = "
    CREATE TEMP TABLE moz_updateoriginsdelete_temp (
        prefix TEXT NOT NULL,
        host TEXT NOT NULL,
        frecency_delta INTEGER NOT NULL,
        PRIMARY KEY (prefix, host)
    ) WITHOUT ROWID
";

// This table is used in a similar way to moz_updateoriginsinsert_temp, but for
// updates to places' frecencies, and triggered via
// moz_places_afterupdate_frecency_trigger.
//
// When rows are added to this table, moz_places.origin_id may be null.  That's
// why this table uses prefix + host as its primary key, not origin_id.
const CREATE_UPDATEORIGINSUPDATE_TEMP: &str = "
    CREATE TEMP TABLE moz_updateoriginsupdate_temp (
        prefix TEXT NOT NULL,
        host TEXT NOT NULL,
        frecency_delta INTEGER NOT NULL,
        PRIMARY KEY (prefix, host)
    ) WITHOUT ROWID
";

// Note: desktop places also has a notion of "frecency decay", and it only runs this
// `WHEN NOT is_frecency_decaying()`.
const CREATE_PLACES_AFTERUPDATE_FRECENCY_TRIGGER: &str = "
    CREATE TEMP TRIGGER moz_places_afterupdate_frecency_trigger
    AFTER UPDATE OF frecency ON moz_places FOR EACH ROW
    BEGIN

        INSERT INTO moz_updateoriginsupdate_temp (prefix, host, frecency_delta)
        VALUES (
            get_prefix(NEW.url),
            get_host_and_port(NEW.url),
            MAX(NEW.frecency, 0) - MAX(OLD.frecency, 0)
        )
        ON CONFLICT(prefix, host) DO UPDATE
        SET frecency_delta = frecency_delta + EXCLUDED.frecency_delta;
    END
";

// Keys in the moz_meta table. Might be cool to replace this with an enum or something for better
// type safety but whatever.
pub(crate) static MOZ_META_KEY_ORIGIN_FRECENCY_COUNT: &'static str = "origin_frecency_count";
pub(crate) static MOZ_META_KEY_ORIGIN_FRECENCY_SUM: &'static str = "origin_frecency_sum";
pub(crate) static MOZ_META_KEY_ORIGIN_FRECENCY_SUM_OF_SQUARES: &'static str =
    "origin_frecency_sum_of_squares";

// This function is a helper for the next several triggers.  It updates the origin
// frecency stats.  Use it as follows.  Before changing an origin's frecency,
// call the function and pass "-" (subtraction) as the argument.  That will update
// the stats by deducting the origin's current contribution to them.  And then
// after you change the origin's frecency, call the function again, this time
// passing "+" (addition) as the argument.  That will update the stats by adding
// the origin's new contribution to them.
fn update_origin_frecency_stats(op: &str) -> String {
    format!(
        "
        INSERT OR REPLACE INTO moz_meta(key, value)
        SELECT
            '{frecency_count}',
            IFNULL((SELECT value FROM moz_meta WHERE key = '{frecency_count}'), 0)
                {op} CAST (frecency > 0 AS INT)
        FROM moz_origins WHERE prefix = OLD.prefix AND host = OLD.host
        UNION
        SELECT
            '{frecency_sum}',
            IFNULL((SELECT value FROM moz_meta WHERE key = '{frecency_sum}'), 0)
                {op} MAX(frecency, 0)
        FROM moz_origins WHERE prefix = OLD.prefix AND host = OLD.host
        UNION
        SELECT
            '{frecency_sum_of_squares}',
            IFNULL((SELECT value FROM moz_meta WHERE key = '{frecency_sum_of_squares}'), 0)
                {op} (MAX(frecency, 0) * MAX(frecency, 0))
        FROM moz_origins WHERE prefix = OLD.prefix AND host = OLD.host
    ",
        op = op,
        frecency_count = MOZ_META_KEY_ORIGIN_FRECENCY_COUNT,
        frecency_sum = MOZ_META_KEY_ORIGIN_FRECENCY_SUM,
        frecency_sum_of_squares = MOZ_META_KEY_ORIGIN_FRECENCY_SUM_OF_SQUARES,
    )
}

// The next several triggers are a workaround for the lack of FOR EACH STATEMENT
// in Sqlite, (see bug 871908).
//
// While doing inserts or deletes into moz_places, we accumulate the affected
// origins into a temp table. Afterwards, we delete everything from the temp
// table, causing the AFTER DELETE trigger to fire for it, which will then
// update moz_origins and the origin frecency stats. As a consequence, we also
// do this for updates to moz_places.frecency in order to make sure that changes
// to origins are serialized.
//
// Note this way we lose atomicity, crashing between the 2 queries may break the
// tables' coherency. So it's better to run those DELETE queries in a single
// transaction. Regardless, this is still better than hanging the browser for
// several minutes on a fast machine.

// Note: unlike the version of this trigger in desktop places, we don't bother with calling
// store_last_inserted_id. Bug comments indicate that's only really needed because the hybrid
// sync/async connection places has prevents `last_insert_rowid` from working. This shouldn't be an
// issue for us, and it's unclear how we'd implement `store_last_inserted_id` it while supporting
// multiple connections to separate databases open simultaneously, which we'd like for testing
// purposes. (To be clear, it's certainly possible to implement it if it turns out we need it, it
// would just be very tricky).
const CREATE_PLACES_AFTERINSERT_TRIGGER_ORIGINS: &str = "
    CREATE TEMP TRIGGER moz_places_afterinsert_trigger_origins
    AFTER INSERT ON moz_places FOR EACH ROW
    BEGIN
        INSERT OR IGNORE INTO moz_updateoriginsinsert_temp (place_id, prefix, host, rev_host, frecency)
        VALUES (
            NEW.id,
            get_prefix(NEW.url),
            get_host_and_port(NEW.url),
            reverse_host(get_host_and_port(NEW.url)),
            NEW.frecency
        );
    END
";

lazy_static! {
    // This trigger corresponds to `CREATE_PLACES_AFTERINSERT_TRIGGER_ORIGINS`.  It runs on deletes on
    // moz_updateoriginsinsert_temp -- logically, after inserts on moz_places.
    static ref CREATE_UPDATEORIGINSINSERT_AFTERDELETE_TRIGGER: String = format!("
        CREATE TEMP TRIGGER moz_updateoriginsinsert_afterdelete_trigger
        AFTER DELETE ON moz_updateoriginsinsert_temp FOR EACH ROW
        BEGIN
            {decrease_frecency_stats};

            INSERT INTO moz_origins (prefix, host, rev_host, frecency)
            VALUES (
                OLD.prefix,
                OLD.host,
                OLD.rev_host,
                MAX(OLD.frecency, 0)
            )
            ON CONFLICT(prefix, host) DO UPDATE
                SET frecency = frecency + OLD.frecency
                WHERE OLD.frecency > 0;

            {increase_frecency_stats};

            UPDATE moz_places SET origin_id = (
                SELECT id
                FROM moz_origins
                WHERE prefix = OLD.prefix
                AND host = OLD.host
            )
            WHERE id = OLD.place_id;
        END",
        decrease_frecency_stats = update_origin_frecency_stats("-"),
        increase_frecency_stats = update_origin_frecency_stats("+"),
    );
    // moz_updateoriginsdelete_temp -- logically, after deletes on moz_places.
    static ref CREATE_UPDATEORIGINSDELETE_AFTERDELETE_TRIGGER: String = format!("
        CREATE TEMP TRIGGER moz_updateoriginsdelete_afterdelete_trigger
        AFTER DELETE ON moz_updateoriginsdelete_temp FOR EACH ROW
        BEGIN
            {decrease_frecency_stats};

            UPDATE moz_origins SET frecency = frecency + OLD.frecency_delta
            WHERE prefix = OLD.prefix AND host = OLD.host;

            DELETE FROM moz_origins
            WHERE prefix = OLD.prefix
              AND host = OLD.host
              AND NOT EXISTS (
                SELECT id FROM moz_places
                WHERE origin_id = moz_origins.id
                LIMIT 1
              );

            {increase_frecency_stats};

            -- Note: this is where desktop places deletes from moz_icons (so long as
            -- the icon is no longer referenced)
        END",
        decrease_frecency_stats = update_origin_frecency_stats("-"),
        increase_frecency_stats = update_origin_frecency_stats("+"),
    );
}

pub fn init(db: &PlacesDb) -> Result<()> {
    let user_version = db.query_one::<i64>("PRAGMA user_version")?;
    if user_version == 0 {
        create(db)?;
    } else if user_version != VERSION {
        if user_version < VERSION {
            upgrade(db, user_version)?;
        } else {
            log::warn!(
                "Loaded future schema version {} (we only understand version {}). \
                 Optimisitically ",
                user_version,
                VERSION
            )
        }
    }
    log::debug!("Creating temp tables and triggers");
    db.execute_all(&[
        &CREATE_TRIGGER_HISTORYVISITS_AFTERINSERT,
        &CREATE_TRIGGER_HISTORYVISITS_AFTERDELETE,
        &CREATE_TRIGGER_MOZPLACES_AFTERDELETE_ORIGINS,
        &CREATE_TRIGGER_MOZPLACES_AFTERDELETE_TOMBSTONES,
        CREATE_TRIGGER_MOZPLACES_AFTERINSERT_REMOVE_TOMBSTONES,
        CREATE_UPDATEORIGINSINSERT_TEMP,
        CREATE_UPDATEORIGINSDELETE_TEMP,
        CREATE_UPDATEORIGINSUPDATE_TEMP,
        CREATE_PLACES_AFTERUPDATE_FRECENCY_TRIGGER,
        CREATE_PLACES_AFTERINSERT_TRIGGER_ORIGINS,
        &*CREATE_UPDATEORIGINSINSERT_AFTERDELETE_TRIGGER,
        &*CREATE_UPDATEORIGINSDELETE_AFTERDELETE_TRIGGER,
    ])?;

    Ok(())
}

// https://github.com/mozilla-mobile/firefox-ios/blob/master/Storage/SQL/LoginsSchema.swift#L100
fn upgrade(_db: &PlacesDb, from: i64) -> Result<()> {
    log::debug!("Upgrading schema from {} to {}", from, VERSION);
    if from == VERSION {
        return Ok(());
    }
    // FIXME https://github.com/mozilla/application-services/issues/438
    // NB: PlacesConnection.kt checks for this error message verbatim as a workaround.
    panic!("sorry, no upgrades yet - delete your db!");
}

pub fn create(db: &PlacesDb) -> Result<()> {
    log::debug!("Creating schema");
    db.execute_all(&[
        CREATE_TABLE_PLACES_SQL,
        CREATE_TABLE_PLACES_TOMBSTONES_SQL,
        CREATE_TABLE_HISTORYVISITS_SQL,
        CREATE_TABLE_INPUTHISTORY_SQL,
        CREATE_TABLE_BOOKMARKS_SQL,
        CREATE_TABLE_ORIGINS_SQL,
        CREATE_TABLE_META_SQL,
        CREATE_IDX_MOZ_PLACES_URL_HASH,
        CREATE_IDX_MOZ_PLACES_VISITCOUNT_LOCAL,
        CREATE_IDX_MOZ_PLACES_VISITCOUNT_REMOTE,
        CREATE_IDX_MOZ_PLACES_FRECENCY,
        CREATE_IDX_MOZ_PLACES_LASTVISITDATE_LOCAL,
        CREATE_IDX_MOZ_PLACES_LASTVISITDATE_REMOTE,
        CREATE_IDX_MOZ_PLACES_GUID,
        CREATE_IDX_MOZ_PLACES_ORIGIN_ID,
        CREATE_IDX_MOZ_HISTORYVISITS_PLACEDATE,
        CREATE_IDX_MOZ_HISTORYVISITS_FROMVISIT,
        CREATE_IDX_MOZ_HISTORYVISITS_VISITDATE,
        CREATE_IDX_MOZ_HISTORYVISITS_ISLOCAL,
        CREATE_IDX_MOZ_BOOKMARKS_PLACELASTMODIFIED,
        &format!("PRAGMA user_version = {version}", version = VERSION),
    ])?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::PlacesDb;
    use crate::types::SyncStatus;
    use sync15::util::random_guid;
    use url::Url;

    fn has_tombstone(conn: &PlacesDb, guid: &str) -> bool {
        let count: Result<Option<u32>> = conn.try_query_row(
            "SELECT COUNT(*) from moz_places_tombstones
                     WHERE guid = :guid",
            &[(":guid", &guid)],
            |row| Ok(row.get_checked::<_, u32>(0)?),
            true,
        );
        count.unwrap().unwrap() == 1
    }

    #[test]
    fn test_places_no_tombstone() {
        let conn = PlacesDb::open_in_memory(None).expect("no memory db");
        let guid = random_guid().expect("should get a guid");

        conn.execute_named_cached(
            "INSERT INTO moz_places (guid, url, url_hash) VALUES (:guid, :url, hash(:url))",
            &[
                (":guid", &guid),
                (
                    ":url",
                    &Url::parse("http://example.com")
                        .expect("valid url")
                        .into_string(),
                ),
            ],
        )
        .expect("should work");

        let place_id = conn.last_insert_rowid();
        conn.execute_named_cached(
            "DELETE FROM moz_places WHERE id = :id",
            &[(":id", &place_id)],
        )
        .expect("should work");

        // should not have a tombstone.
        assert!(!has_tombstone(&conn, &guid));
    }

    #[test]
    fn test_places_tombstone_removal() {
        let conn = PlacesDb::open_in_memory(None).expect("no memory db");
        let guid = random_guid().expect("should get a guid");

        conn.execute_named_cached(
            "INSERT INTO moz_places_tombstones VALUES (:guid)",
            &[(":guid", &guid)],
        )
        .expect("should work");

        // insert into moz_places - the tombstone should be removed.
        conn.execute_named_cached(
            "INSERT INTO moz_places (guid, url, url_hash, sync_status)
             VALUES (:guid, :url, hash(:url), :sync_status)",
            &[
                (":guid", &guid),
                (
                    ":url",
                    &Url::parse("http://example.com")
                        .expect("valid url")
                        .into_string(),
                ),
                (":sync_status", &SyncStatus::Normal),
            ],
        )
        .expect("should work");
        assert!(!has_tombstone(&conn, &guid));
    }
}
