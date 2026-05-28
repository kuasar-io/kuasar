ROOTFS=/tmp/unified-bench-kuasar/rootfs-slim.raw
echo "$ROOTFS"

truncate -s 200M $ROOTFS
mkfs.ext4 $ROOTFS
mount -o loop $ROOTFS /mnt
CID=$(docker create nginx-slim:0.21)
docker export $CID | tar -xf - -C /mnt
docker rm $CID


cat > /mnt/init << 'EOF'
#!/bin/sh
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
/usr/sbin/nginx 2>/dev/null
while true; do
    echo "READY" > /dev/ttyS0
    sleep 0.1
done
EOF
chmod +x /mnt/init

cat /mnt/init

umount /mnt
