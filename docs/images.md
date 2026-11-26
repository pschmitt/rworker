# Worker images

## Build and upload

```sh
rworker image create
rworker image create --upload --cloud builds --name my-worker
rworker image upload --cloud builds --name my-worker ./worker.qcow2
```

`image create` builds the standalone x86-64 NixOS image from the rworker
flake and prints the image file's Nix store path. Use
`--flake .` from a local checkout or
`--flake 'path:./my-flake#my-worker-image'` for a custom output.
A flake reference without an attribute selects
`nixosConfigurations.rworker.config.image.repart.image`.

Uploads use the local `openstack` CLI and create private images.
`--cloud` selects a named entry in `clouds.yaml` and takes precedence over
`OS_CLOUD`. Use the cloud entry already scoped to your target project for
ordinary uploads. `--project NAME_OR_ID` sets image ownership when your
credentials have permission to assign an alternate owner. `--tag TAG`
adds an image tag when you want your own provisioning workflow to select it.
Both image commands support `--json`.

## Boot the standalone image over a floating IP

The standalone image uses the OpenStack cloud-init datasource. It reads
the instance key pair from metadata and authorizes it for the `rworker`
user, the login account of every rworker worker. The image ships with no baked-in public keys. The SSH service
accepts key authentication.

Use a security group that allows TCP port 22 from your client network
and a VirtIO boot volume with room for builds. Substitute your image,
flavor, networks, security group and key pair in the example. [OpenStack server commands](https://docs.openstack.org/python-openstackclient/2026.1/cli/command-objects/server.html)

```sh
export OS_CLOUD=builds
openstack server create --image my-worker --flavor build \
  --boot-from-volume 40 --network tenant --security-group restricted-ssh \
  --key-name my-key --wait my-worker
ip=$(openstack floating ip create public -f value -c floating_ip_address)
openstack server add floating ip my-worker "$ip"
```

Verify the SSH host key with the server console or a trusted management
path before connecting as `rworker`. OpenStack keeps a boot volume created
with `--boot-from-volume` after server deletion. Release the floating IP
and delete the volume when you are done with them. [OpenStack floating IP guide](https://docs.openstack.org/install-guide/launch-instance-selfservice.html)

Floating IP allocation and association are OpenStack operations. The image
subcommands build and upload images. The rworker control service's worker
provisioning uses its configured overlay networking.

## Reuse the base in your NixOS flake

The [sample worker image flake](../examples/worker-image/flake.nix) builds
a standalone OpenStack image with an extra package. Copy it into your site
flake or build the example directly from this checkout.

```sh
rworker image create --flake path:./examples/worker-image
```

Import `inputs.rworker.nixosModules.workerImage` for the UEFI image and
boot-time encrypted writable Nix store. Supply your own accounts, key
provisioning and network configuration. The image is available at
`config.image.repart.image`.

Import `inputs.rworker.nixosModules.workerImageOpenStack` to include the
standalone cloud-init and SSH defaults. Set `system.stateVersion` in your host
configuration. Keep site-specific settings in your own flake.
