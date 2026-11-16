-- Follows a fold head: what each fresh journal row counts under the
-- measures $2 names, as the CTE `facts`: the measure and key, the row's UTC
-- day, how many rows and what amount it adds, and the wallet it names, if
-- any. `scope` is the chain whose presence rows tell the wallet's days:
-- chain $1, or chain 0 for the users across every chain.
--
-- Every row counts under `users` but a referral and a bind fee's setting,
-- which the splitter logs beside the bind's own fee: its wallet when it
-- binds, sends through the escrow or claims from it, unless the wallet is a
-- referral splitter any chain watches, which sends every paid share. Its
-- deposits and forwards still count. Every movement counts under its token and
-- under every token together, a count only. Every fee counts under its
-- receiver and under every receiver together, with its amount: fees are
-- paid in the chain's own coin. A paid referral counts its share, with no
-- key, under where the escrow put it at the bind as its referral row tells:
-- `shared_forwarded`, or `shared_escrowed` when held, claimed since or not.
-- The window writes that row before it folds. A claim onboards when the
-- claimed round took a deposit of the token while nobody held the node, and
-- the node was bound after that deposit and before the claim: the binds are
-- tested against the round's first deposit alone, one seek each.
, splitters AS MATERIALIZED (
    SELECT referral_splitter AS wallet
    FROM names.chains
    WHERE referral_splitter IS NOT NULL
), seen AS (
    SELECT kind, platform_id, receiver, amount,
           block_time - block_time % 86400 AS day,
           CASE WHEN kind = 'identity_bound' THEN holder
                WHEN kind IN ('deposited', 'forwarded')
                     AND sender NOT IN (SELECT wallet FROM splitters) THEN sender
                WHEN kind = 'claimed' THEN claimer END AS wallet
    FROM fresh
    WHERE kind NOT IN ('referred', 'bind_fee_set')
), moves AS (
    SELECT kind AS flow, token,
           CASE WHEN kind = 'claimed' THEN claimer
                WHEN sender NOT IN (SELECT wallet FROM splitters) THEN sender
                END AS wallet,
           CASE WHEN kind IN ('claimed', 'refunded') THEN released
                ELSE amount END AS amount,
           block_time - block_time % 86400 AS day
    FROM fresh
    WHERE kind IN ('deposited', 'forwarded', 'claimed', 'refunded')
    UNION ALL
    SELECT 'onboarding', c.token, c.claimer, c.released,
           c.block_time - c.block_time % 86400
    FROM fresh c
    WHERE c.kind = 'claimed'
      AND EXISTS (
          SELECT 1
          FROM names.events b
          WHERE b.handle_node = c.handle_node
            AND b.chain_id = c.chain_id
            AND b.kind = 'identity_bound'
            AND (b.block_number, b.log_index) < (c.block_number, c.log_index)
            AND (b.block_number, b.log_index) > (
                SELECT d.block_number, d.log_index
                FROM names.events d
                WHERE d.handle_node = c.handle_node
                  AND d.chain_id = c.chain_id
                  AND d.kind = 'deposited'
                  AND d.token = c.token
                  AND d.round = c.round
                ORDER BY d.block_number, d.log_index
                LIMIT 1))
), facts AS MATERIALIZED (
    SELECT *
    FROM (
        SELECT $1::bigint AS scope, 'users'::text AS measure, ''::bytea AS key,
               day, 1::bigint AS count, 0::numeric AS amount, wallet
        FROM seen
        UNION ALL
        SELECT 0, 'all_chains_users', '', day, 0, 0, wallet
        FROM seen
        WHERE wallet IS NOT NULL
        UNION ALL
        SELECT $1, 'links', platform_id, day, 1, 0, wallet
        FROM seen
        WHERE kind = 'identity_bound'
        UNION ALL
        SELECT $1, 'fees', receiver, day, 1, amount, NULL
        FROM seen
        WHERE kind = 'bind_fee_paid'
        UNION ALL
        SELECT $1, 'fees', '', day, 1, amount, NULL
        FROM seen
        WHERE kind = 'bind_fee_paid'
        UNION ALL
        SELECT $1, flow, token, day, 1, amount, wallet
        FROM moves
        UNION ALL
        SELECT $1, flow, '', day, 1, 0, NULL
        FROM moves
        UNION ALL
        SELECT $1,
               CASE WHEN r.delivery = 'forwarded' THEN 'shared_forwarded'
                    ELSE 'shared_escrowed' END,
               '', e.block_time - e.block_time % 86400, 1, r.share, NULL
        FROM fresh e
        JOIN names.referrals r
          ON r.chain_id = e.chain_id AND r.block_number = e.block_number
             AND r.log_index = e.log_index
        WHERE e.kind = 'referred' AND r.outcome = 'paid'
    ) f
    WHERE measure = ANY($2::text[])
)
