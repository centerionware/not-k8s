#!/usr/bin/env bash
# remove-cloudprovider-taint.sh — one-time fix for a node stuck Unschedulable
# from before nodelet stopped setting spec.providerID (which triggers this
# taint, expecting a cloud-controller-manager — disabled on purpose here,
# there is no cloud provider — to clear it; nothing ever did). Only needed
# once per node registered by an older nodelet build; new registrations
# never get this taint in the first place.
#
# Usage:
#   ./deploy/remove-cloudprovider-taint.sh [node-name]   # default: `hostname`
kubectl taint nodes "${1:-$(hostname)}" node.cloudprovider.kubernetes.io/uninitialized-
