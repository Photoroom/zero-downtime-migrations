# R005: RemoveField without SeparateDatabaseAndState - should fail
from django.db import migrations


class Migration(migrations.Migration):

    dependencies = [
        ('myapp', '0001_initial'),
    ]

    operations = [
        migrations.RemoveField(
            model_name='product',
            name='deprecated_field',
        ),
    ]
