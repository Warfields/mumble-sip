CREATE TABLE IF NOT EXISTS caller_channels (
    phone_number TEXT NOT NULL,
    server_host  TEXT NOT NULL,
    channel_id   INTEGER NOT NULL,
    PRIMARY KEY (phone_number, server_host),
    FOREIGN KEY (phone_number) REFERENCES callers(phone_number)
);
