-- Cutover probes from the 2026-09-14 register flip. Exact emails / install ids only.
DELETE FROM pings WHERE install_id IN ('i-preflight', 'i-cutover', 'i-post');
DELETE FROM events WHERE install_id IN ('i-preflight', 'i-cutover', 'i-post');
DELETE FROM users WHERE email IN (
  'preflight1789422750@example.com',
  'cutover1789422868@example.com',
  'postcut1789423401@example.com'
);
