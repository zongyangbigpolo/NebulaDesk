-- Machines are placed on gateways, and a gateway on the wrong continent turns
-- a local session into a transcontinental one. The region is decided by the
-- operator when they issue an enrolment token, not by the machine: a machine
-- that could choose its own region could pull sessions onto a gateway of its
-- choosing.
ALTER TABLE enrollment_tokens ADD COLUMN region TEXT NOT NULL DEFAULT 'default';
ALTER TABLE machines          ADD COLUMN region TEXT NOT NULL DEFAULT 'default';

CREATE INDEX machines_region_idx ON machines (region);
