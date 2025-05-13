-- Database bootstrap
-- Partitioned by first_char with composite PK and automatic partition & trigger setup

-- Enable PL/pgSQL (for DO blocks)
CREATE EXTENSION IF NOT EXISTS plpgsql;

------------------------------------------------------------
-- Trigger function
------------------------------------------------------------
CREATE OR REPLACE FUNCTION slugs_firstchar_trigger()
RETURNS TRIGGER AS $$
BEGIN
    NEW.first_char := substring(NEW.slug, 1, 1);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

------------------------------------------------------------
-- Parent table
------------------------------------------------------------
CREATE TABLE IF NOT EXISTS slugs (
    slug       VARCHAR(256)  NOT NULL,
    first_char VARCHAR(1)  NOT NULL,
    url        TEXT        NOT NULL,
    owner      TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT slugs_pk PRIMARY KEY (first_char, slug)
) PARTITION BY LIST (first_char);

-- Index on created_at for TTL/analytics
CREATE INDEX IF NOT EXISTS slugs_created_at_idx ON slugs(created_at);

------------------------------------------------------------
-- Create partitions & attach triggers
------------------------------------------------------------
DO $$
DECLARE
    ch VARCHAR(1);
BEGIN
    -- create partitions and attach triggers for all characters 0-9, A-Z, a-z
    FOREACH ch IN ARRAY string_to_array('0#1#2#3#4#5#6#7#8#9#A#B#C#D#E#F#G#H#I#J#K#L#M#N#O#P#Q#R#S#T#U#V#W#X#Y#Z#a#b#c#d#e#f#g#h#i#j#k#l#m#n#o#p#q#r#s#t#u#v#w#x#y#z', '#') LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS "slugs_%1$s" PARTITION OF slugs FOR VALUES IN (''%1$s'');',
            ch
        );
        EXECUTE format(
            'DROP TRIGGER IF EXISTS "trg_set_first_char_%1$s" ON "slugs_%1$s";',
            ch
        );
        EXECUTE format(
            'CREATE TRIGGER "trg_set_first_char_%1$s" BEFORE INSERT OR UPDATE ON "slugs_%1$s" FOR EACH ROW EXECUTE FUNCTION slugs_firstchar_trigger();',
            ch
        );
    END LOOP;
END;
$$ LANGUAGE plpgsql;
