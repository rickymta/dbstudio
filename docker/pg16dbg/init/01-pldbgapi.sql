-- Debugger API used by the SQL debugger (needs `plugin_debugger` in shared_preload_libraries).
CREATE EXTENSION IF NOT EXISTS pldbgapi;

-- Sample routines for the routine debugger tests (package P extends this file as needed).
CREATE OR REPLACE FUNCTION public.f_inner(x integer) RETURNS integer
LANGUAGE plpgsql AS $$
DECLARE
  r integer;
BEGIN
  r := x * 2;
  RETURN r;
END
$$;

CREATE OR REPLACE FUNCTION public.f_outer(x integer) RETURNS integer
LANGUAGE plpgsql AS $$
DECLARE
  total integer := 0;
  i integer;
BEGIN
  FOR i IN 1..x LOOP
    total := total + public.f_inner(i);
  END LOOP;
  RETURN total;
END
$$;
