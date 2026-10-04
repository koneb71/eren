-- What a person carries from project to project.
--
-- Rules: the text written as AGENTS.md (and a CLAUDE.md that imports it) into
-- each new repository project, so every agent CLI reads it there. Kept on the
-- account; with accounts off it is the `rules` settings key, and the first
-- admin adopts it (see `eren_core::rules`).
ALTER TABLE users ADD COLUMN rules TEXT NOT NULL DEFAULT '';

-- Skills: a skill with no workspace is personal — offered in every workspace
-- its owner has, and still applied only where it is named. owner_id is NULL
-- with accounts off (the one local person), and the first admin adopts those.
ALTER TABLE skills ALTER COLUMN workspace_id DROP NOT NULL;
ALTER TABLE skills ADD COLUMN owner_id UUID REFERENCES users(id) ON DELETE CASCADE;
CREATE UNIQUE INDEX skills_personal_name
    ON skills (COALESCE(owner_id, '00000000-0000-0000-0000-000000000000'::uuid), lower(name))
    WHERE workspace_id IS NULL;

-- Whether a skill can be named in a workspace: its own, or a personal one of
-- the workspace's owner. One definition, so the library, the @ picker, the
-- name checks and the card check cannot disagree about it.
CREATE FUNCTION skill_in_workspace(skill_ws uuid, skill_owner uuid, ws uuid)
RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT skill_ws = ws
        OR (skill_ws IS NULL
            AND skill_owner IS NOT DISTINCT FROM (SELECT owner_id FROM workspaces WHERE id = ws))
$$;
