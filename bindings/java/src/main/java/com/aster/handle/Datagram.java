package com.aster.handle;

/**
 * An incoming datagram received on a connection.
 *
 * @param data the datagram payload bytes
 */
public record Datagram(byte[] data) {}
