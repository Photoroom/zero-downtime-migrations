# R003: RunSQL with CREATE INDEX - should fail
from django.db import migrations


class Migration(migrations.Migration):

    dependencies = [
        ('myapp', '0005'),
    ]

    operations = [
        migrations.RunSQL(
            sql='CREATE INDEX order_status_idx ON myapp_order (status);',
            reverse_sql='DROP INDEX order_status_idx;',
        ),
    ]
