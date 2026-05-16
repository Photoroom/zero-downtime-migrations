# R013: RunSQL without reverse_sql - should fail (warning)
from django.db import migrations


class Migration(migrations.Migration):

    dependencies = [
        ('myapp', '0001_initial'),
    ]

    operations = [
        migrations.RunSQL(
            sql="UPDATE product SET active = TRUE WHERE active IS NULL;",
        ),
    ]
