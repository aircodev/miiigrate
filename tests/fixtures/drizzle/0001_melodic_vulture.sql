ALTER TABLE "users" ADD COLUMN "name" text;--> statement-breakpoint
CREATE INDEX "orders_user_id_idx" ON "orders" USING btree ("user_id");
