CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER);
CREATE TABLE message (id TEXT, session_id TEXT, data TEXT);
CREATE TABLE part (id TEXT, message_id TEXT, data TEXT);
INSERT INTO session VALUES ('ses_v1', NULL, '/work/legacy', 'Legacy session', 1767225600000, 1767225602000);
INSERT INTO message VALUES ('m_user', 'ses_v1', '{"id":"m_user","role":"user","time":{"created":1767225600000}}');
INSERT INTO message VALUES ('m_assistant', 'ses_v1', '{"id":"m_assistant","role":"assistant","modelID":"model-v1","time":{"created":1767225601000,"completed":1767225602000},"tokens":{"input":10,"output":5,"reasoning":2,"cache":{"read":3,"write":1}}}');
INSERT INTO part VALUES ('p_user', 'm_user', '{"type":"text","messageID":"m_user","text":"Legacy prompt"}');
