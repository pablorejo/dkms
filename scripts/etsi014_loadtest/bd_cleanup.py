import os, psycopg, urllib.parse as up
url = os.getenv('DB_URL', '').replace('postgresql://', '')
parsed = up.urlparse('postgresql://' + url)
conn_str = f'host={parsed.hostname} port={parsed.port or 5432} dbname={parsed.path.lstrip("/")} user={parsed.username} password={parsed.password}'
with psycopg.connect(conn_str, autocommit=True) as c, c.cursor() as cur:
    cur.execute('DELETE FROM kme'); print(f'kmes: {cur.rowcount}')
    cur.execute("""DELETE FROM dkms WHERE id_host IN (SELECT id FROM host WHERE id_simulation IN (SELECT id FROM simulation WHERE status='finished'))"""); print(f'dkms: {cur.rowcount}')
    cur.execute("""DELETE FROM orr WHERE id_host IN (SELECT id FROM host WHERE id_simulation IN (SELECT id FROM simulation WHERE status='finished'))"""); print(f'orr: {cur.rowcount}')
    cur.execute("""DELETE FROM sdn WHERE id_host IN (SELECT id FROM host WHERE id_simulation IN (SELECT id FROM simulation WHERE status='finished'))"""); print(f'sdn: {cur.rowcount}')
    cur.execute("""DELETE FROM qkc WHERE id_host IN (SELECT id FROM host WHERE id_simulation IN (SELECT id FROM simulation WHERE status='finished'))"""); print(f'qkc: {cur.rowcount}')
    cur.execute("""DELETE FROM host WHERE id_simulation IN (SELECT id FROM simulation WHERE status='finished')"""); print(f'host: {cur.rowcount}')
    cur.execute('SELECT count(*) FROM kme'); print(f'kmes after: {cur.fetchone()[0]}')
    cur.execute('SELECT count(*) FROM host'); print(f'hosts after: {cur.fetchone()[0]}')
