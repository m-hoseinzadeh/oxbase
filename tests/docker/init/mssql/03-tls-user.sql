-- M4 (docs/m4-contracts.md §5.2): login oxbase_tls, accepted only over TLS. SQL Server cannot
-- require TLS per login and forceencryption = 1 would require it for every login, so a server
-- LOGON trigger rolls back any oxbase_tls login whose connection is not encrypted (the client
-- then gets error 17892). CONNECTIONPROPERTY needs no VIEW SERVER STATE.
-- Server-level objects survive a reseed of oxbase_test, so every step is re-runnable.
USE master;
GO
IF SUSER_ID(N'oxbase_tls') IS NULL
    CREATE LOGIN oxbase_tls WITH PASSWORD = N'Oxbase_Fixture_Pw_2026', CHECK_POLICY = OFF,
        DEFAULT_DATABASE = oxbase_test;
GO
USE oxbase_test;
GO
IF USER_ID(N'oxbase_tls') IS NULL
    CREATE USER oxbase_tls FOR LOGIN oxbase_tls;
ALTER ROLE db_owner ADD MEMBER oxbase_tls;
GO
USE master;
GO
IF EXISTS (SELECT 1 FROM sys.server_triggers WHERE name = N'oxbase_tls_only')
    DROP TRIGGER oxbase_tls_only ON ALL SERVER;
GO
CREATE TRIGGER oxbase_tls_only ON ALL SERVER FOR LOGON AS
BEGIN
    IF ORIGINAL_LOGIN() = N'oxbase_tls'
       AND CONVERT(nvarchar(10), CONNECTIONPROPERTY('encrypt_option')) <> N'TRUE'
        ROLLBACK;
END;
GO
