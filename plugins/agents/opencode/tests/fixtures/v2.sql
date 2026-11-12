CREATE TABLE session_v2 (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER);
CREATE TABLE session_message (id TEXT, session_id TEXT, type TEXT, seq INTEGER, data TEXT);
INSERT INTO session_v2 VALUES ('ses_v2', NULL, '/work/v2', 'V2 session', 1767225600000, 1767225602000);
INSERT INTO session_v2 VALUES ('ses_child', 'ses_v2', '/work/v2', 'Child session - helper', 1767225600500, 1767225602500);
INSERT INTO session_message VALUES ('m_user', 'ses_v2', 'user', 1, '{"text":"V2 prompt","time":{"created":1767225600000}}');
INSERT INTO session_message VALUES ('m_assistant', 'ses_v2', 'assistant', 2, '{"model":{"id":"model-v2","providerID":"fixture"},"time":{"created":1767225601000,"completed":1767225602000},"tokens":{"input":20,"output":7,"reasoning":3,"cache":{"read":4,"write":2}}}');
INSERT INTO session_message VALUES ('m_child', 'ses_child', 'assistant', 1, '{"model":{"id":"model-child"},"time":{"created":1767225601500,"completed":1767225602500},"tokens":{"input":2,"output":1,"reasoning":0,"cache":{"read":0,"write":0}}}');
