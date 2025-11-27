# Initial setup of TFTP for iPXE

## Overview

Steps that were used on Armbian to setup TFTP for use with iPXE scripts.

## Steps

- Switch to root and enable r/w mode

```bash
sudo -i
rw
```

- Update package repos

```bash
apt update
```

- Install the TFTP server package

```bash
apt install tftpd-hpa
```

- Configure the TFTP server by editing the default configuration file

```bash
vim /etc/default/tftpd-hpa
```

- Update the file with the following contents (adjust as needed):

```bash
TFTP_USERNAME="tftp"
TFTP_DIRECTORY="/mnt/tftp"
TFTP_ADDRESS=":69"
TFTP_OPTIONS="--secure -v"
```

- Set up the TFTP directory

```bash
mkdir -p /mnt/tftp
chown -R tftp:tftp /mnt/tftp
chmod -R 755 /mnt/tftp
```

- Restart and enable the TFTP service

```bash
systemctl restart tftpd-hpa
systemctl enable tftpd-hpa
```

- Check the service status

```bash
systemctl status tftpd-hpa
```

- Test the TFTP server

```bash
echo "hello, world!" > /mnt/hdd/tftpboot/test.txt
tftp localhost
tftp> get test.txt
tftp> quit
```

- Install caddy to serve files over http

```bash
apt install -y debian-keyring debian-archive-keyring apt-transport-https curl
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | sudo tee /etc/apt/sources.list.d/caddy-stable.list
chmod o+r /usr/share/keyrings/caddy-stable-archive-keyring.gpg
chmod o+r /etc/apt/sources.list.d/caddy-stable.list
apt update
apt install caddy
```

- Create the caddy configuration

```
cat <<-EOF > /etc/caddy/Caddyfile
:80 {
    root * /mnt/hdd/tftpboot

    file_server
}
EOF
```

- Add caddy user to tftp group

```bash
usermod -aG tftp caddy
```

- Reload Caddy

```bash
sudo systemctl reload caddy
```

- Enable on startup

```bash
sudo systemctl enable --now caddy
```

- Check status

```bash
sudo systemctl status caddy
```

## Preparing for iPXE Chaining

- Prepare [iPXE](./IPXE.md]

- Create your iPXE script and name it `config.ipxe`

- Configure your DHCP server (e.g., UniFi UDM Pro)
  - Set TFTP Server to your device's IP.
  - Set Network Boot to the filename (e.g., `boot/x64/ipxe.efi`).

