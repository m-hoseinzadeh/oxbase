CREATE SEQUENCE "m8_tgt"."new_seq" AS bigint INCREMENT BY 1 MINVALUE 1 MAXVALUE 9999999 START WITH 1 NO CYCLE;
ALTER TABLE "m8_tgt"."orders" ALTER COLUMN "id" DROP DEFAULT;
ALTER TABLE "m8_tgt"."orders" ALTER COLUMN "id" SET DEFAULT nextval('m8_tgt.new_seq'::regclass);
DROP SEQUENCE "m8_tgt"."old_seq";
