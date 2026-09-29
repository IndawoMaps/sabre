mkdir -p bench/data bench/out/logs

docker compose -f bench/docker-compose.yml run --rm --no-deps --user "$(id -u):$(id -g)" \
    --entrypoint python titiler /bench/make_cog.py --out /bench/data/dem.tif --size 4096

# Start the stack. BENCH_CPUS caps both servers and sets their worker counts.
BENCH_CPUS=4 docker compose -f bench/docker-compose.yml up --build -d

# 3. Run it
python3 bench/run.py
python3 bench/run.py --cold            # also restart each container and time its first tile


