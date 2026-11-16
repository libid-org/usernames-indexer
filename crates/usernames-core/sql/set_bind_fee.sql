-- Under the window's hold on the row.
UPDATE names.chains SET bind_fee = $2::numeric WHERE chain_id = $1
