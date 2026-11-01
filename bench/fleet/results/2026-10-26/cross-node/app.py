# A heap with known contents and a heartbeat, to tell a good restore from a bad one.
import hashlib, os, time
import numpy as np

heap = np.random.default_rng(42).random(128 * 1024 * 1024 // 8)
ref = hashlib.sha256(heap.tobytes()).hexdigest()[:16]
n = 0
while True:
    n += 1
    # Re-hash the whole heap every few beats, and do real numpy work (BLAS).
    cur = hashlib.sha256(heap.tobytes()).hexdigest()[:16] if n % 3 == 0 else "-"
    dot = float(heap[:4096] @ heap[4096:8192])
    with open("/tmp/state", "w") as f:
        f.write(f"beat={n} ref={ref} now={cur} dot={dot:.6f} t={time.time():.1f}\n")
    time.sleep(1)
