import concurrent.futures
import urllib.request
import time

URL = "http://127.0.0.1:8080/"
TOTAL_REQUESTS = 5000
CONCURRENCY = 100

def send_request(_):
    try:
        start = time.time()
        req = urllib.request.Request(URL, headers={"Host": "localhost"})
        with urllib.request.urlopen(req, timeout=5) as response:
            status = response.status
            return status == 200, time.time() - start
    except Exception:
        return False, 0

print(f"Starting load test: {TOTAL_REQUESTS} requests, concurrency: {CONCURRENCY}...")
start_time = time.time()

success_count = 0
fail_count = 0

with concurrent.futures.ThreadPoolExecutor(max_workers=CONCURRENCY) as executor:
    results = list(executor.map(send_request, range(TOTAL_REQUESTS)))

duration = time.time() - start_time

for success, _ in results:
    if success:
        success_count += 1
    else:
        fail_count += 1

availability = (success_count / TOTAL_REQUESTS) * 100
req_per_sec = TOTAL_REQUESTS / duration

print("\n--- Test Results ---")
print(f"Total time:       {duration:.2f} seconds")
print(f"Requests/sec:     {req_per_sec:.2f}")
print(f"Successful:       {success_count}")
print(f"Failed/Errors:    {fail_count}")
print(f"Availability:     {availability:.2f}% (Target: >= 99.5%)")