# Step 1: Set up environment variables
TYPE=slim 
SERIAL=$W/serial-${TYPE}.log
SNAP=$W/snap-${TYPE}-externalUFFD
SOCK=$W/api-snap-${TYPE}.sock
mkdir -p $SNAP
rm -f $SERIAL $SOCK

# Step 2: Start VM
$CH --api-socket $SOCK \
    --kernel $KERNEL \
    --memory "size=4096M" \
    --cpus "boot=1" \
    --disk "path=$W/rootfs-${TYPE}.raw,readonly=off,direct=off,image_type=raw" \
    --cmdline "root=/dev/vda rw console=ttyS0 init=/init" \
    --serial "file=$SERIAL" \
    --console off &
CH_PID=$!

# Step 3: Waiting for READY(slim: ~1 second）
while ! grep -q "READY" $SERIAL 2>/dev/null; do sleep 0.5; done
echo "nginx READY"

#  Step 4: Pause → Snapshot → Close 
curl --unix-socket $SOCK -s -X PUT http://localhost/api/v1/vm.pause
sleep 1
curl --unix-socket $SOCK -s -H "Content-Type: application/json" \
    -X PUT http://localhost/api/v1/vm.snapshot \
    -d "{\"destination_url\": \"file://$SNAP\"}"
sleep 3
curl --unix-socket $SOCK -s -X PUT http://localhost/api/v1/vm.shutdown
sleep 1
kill -9 $CH_PID 2>/dev/null; wait $CH_PID 2>/dev/null
rm -f $SOCK

# Step 5: Check snapshot
ls -alg $SNAP

